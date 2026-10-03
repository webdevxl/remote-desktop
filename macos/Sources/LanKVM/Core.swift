import Accessibility
import AppKit
import ApplicationServices
import CLanKVM
import Combine
import Foundation
import os

/// Swift face of the Rust core: state for the UI plus the actions it can take.
@MainActor
final class CoreModel: ObservableObject {
    static let shared = CoreModel()

    @Published private(set) var startError: String?
    @Published private(set) var thisMac: ThisMac?
    @Published private(set) var host = HostStatus()
    @Published private(set) var paired = PairedDevices()
    @Published private(set) var recents: [RecentHost] = []
    /// Authoritative Screen Recording state (nil while checking).
    @Published private(set) var canShareScreen: Bool?

    private var sessions: [UInt64: SessionModel] = [:]

    private init() {}

    /// Sample state for UI snapshots (`--snapshot`), without starting the network core.
    func loadSampleState(screenAllowed: Bool) {
        thisMac = ThisMac(name: "Alex's MacBook Pro", addresses: ["192.168.1.23", "10.0.0.7"], port: 47800, deviceId: "724e:b7c8:8a63:ada8")
        host = HostStatus(
            viewers: [Viewer(id: 1, name: "Studio", address: "192.168.1.31", deviceId: "9f12:0ab3:77c1:e402", controlling: screenAllowed)],
            pairing: [],
            screenCaptureAllowed: screenAllowed,
            allowControl: true,
            controlPermission: screenAllowed
        )
        paired = PairedDevices(
            viewers: [PairedDevice(fingerprint: "aa", deviceId: "9f12:0ab3:77c1:e402", name: "Studio")],
            hosts: [
                PairedDevice(fingerprint: "bb", deviceId: "41de:93a0:c2f7:118b", name: "Mac mini"),
                PairedDevice(fingerprint: "cc", deviceId: "07b9:5c2e:a1d4:6f30", name: "Studio"),
            ]
        )
        recents = [
            RecentHost(address: "192.168.1.40", name: "Mac mini"),
            RecentHost(address: "192.168.1.31", name: "Studio"),
        ]
        canShareScreen = screenAllowed
    }

    func start() {
        guard thisMac == nil, startError == nil else { return }
        if let error = takeString(lk_start(coreEventCallback, nil)) {
            startError = error
            return
        }
        thisMac = decode(ThisMac.self, lk_this_mac())
        refreshHost()
        refreshPaired()
        refreshRecents()
        verifyScreenCapture()
        // Test copies (scripts/e2e-control.sh) must never pop permission dialogs.
        let noPrompts = ProcessInfo.processInfo.environment["LANKVM_NO_PROMPTS"] == "1"
        if !lk_screen_capture_allowed() && !noPrompts {
            // Shows the system prompt the first time; macOS ignores later requests.
            _ = lk_request_screen_capture()
        }
    }

    // MARK: Events

    fileprivate func handle(_ event: CoreEvent) {
        switch event.type {
        case .hostChanged:
            let hadRequests = !host.pairing.isEmpty
            refreshHost()
            if !hadRequests && !host.pairing.isEmpty {
                // Someone is waiting for the PIN shown here; make sure it's seen.
                NSApp.activate(ignoringOtherApps: true)
            }
        case .trustChanged:
            refreshPaired()
        case .pinNeeded, .connected, .ended, .control, .cursorShape, .cursor:
            guard let id = event.session, let session = sessions[id] else { return }
            switch event.type {
            case .pinNeeded: session.phase = .needsPin
            case .connected:
                if let info = event.info {
                    session.phase = .connected(info)
                    session.wasConnected = true
                    session.hostId = info.hostId
                    session.sameMachine = info.sameMachine
                    // Pick up where the user left off with this Mac.
                    if UserDefaults.standard.string(forKey: Self.modeKey(info.hostId)) == "control" {
                        setControl(true, for: id, remember: false)
                    }
                }
                refreshRecents()
            case .control:
                let request = event.request ?? 0
                // The core already drops answers to replaced requests; this is the UI's own guard.
                guard request == 0 || request == session.latestRequest else { return }
                // Control is never granted unasked (the core checks this too).
                if event.active == true && session.mode != .control { return }
                session.injectedTag = event.injectedTag ?? 0
                session.hostPid = event.hostPid ?? 0
                if event.active == true {
                    if session.control != .active {
                        session.showToast("Controlling \(session.hostName) · press ⌃⌥⌘ to release")
                    }
                    session.control = .active
                    session.mode = .control
                    // A fresh grant resends the cursor shapes, numbered from the start.
                    session.cursor.clear()
                } else if request != 0 || session.control == .active || session.control == .requesting {
                    let reason = event.reason ?? ControlReason.none
                    session.control = reason == ControlReason.none ? .off : .refused(reason, event.message ?? "")
                    session.mode = .view
                    session.cursor.reset()
                }
            case .cursorShape:
                if let shape = event.id, let png = event.png {
                    session.cursor.add(id: shape, png: png, width: event.width ?? 0, height: event.height ?? 0,
                                       hotX: event.hotX ?? 0, hotY: event.hotY ?? 0)
                }
            case .cursor:
                switch event.state {
                case "shape": if let shape = event.id { session.cursor.hostReported(.shape(shape)) }
                case "hidden": session.cursor.hostReported(.hidden)
                default: session.cursor.hostReported(.inVideo)
                }
            default:
                session.phase = .ended(event.error)
                session.control = .off
                session.mode = .view
            }
        }
    }

    func refreshHost() {
        if let status = decode(HostStatus.self, lk_host_status()), status != host {
            host = status
            // ⌃⌥⌘. takes control back, but only claims the shortcut while someone controls.
            StopControlHotKey.shared.setEnabled(status.controller != nil) { [weak self] in
                self?.stopAllControl()
            }
            // The Mac now controlling this one can't also be controlled from here: input would
            // bounce between the two. The newer control wins; this window goes back to viewing.
            if let controller = status.controller {
                for (id, session) in sessions where session.hostId == controller.deviceId
                    && (session.control == .active || session.control == .requesting) {
                    setControl(false, for: id, remember: false)
                    session.showToast("\(controller.name) is controlling this Mac, so this window only views it")
                }
            }
        }
    }

    func refreshPaired() {
        paired = decode(PairedDevices.self, lk_paired_devices()) ?? PairedDevices()
    }

    func refreshRecents() {
        recents = decode([RecentHost].self, lk_recent_hosts()) ?? []
    }

    func verifyScreenCapture() {
        canShareScreen = nil
        Task.detached {
            let ok = lk_verify_screen_capture()
            await MainActor.run { CoreModel.shared.canShareScreen = ok }
        }
    }

    // MARK: Viewer sessions

    /// Starts a session and returns its id, to open a viewer window for.
    func connect(to target: String) -> UInt64 {
        let screen = NSScreen.main ?? NSScreen.screens.first
        let scale = screen?.backingScaleFactor ?? 2
        let size = screen?.frame.size ?? CGSize(width: 1920, height: 1080)
        // ProMotion screens run at 120 Hz: frames twice as often means input shows up sooner.
        let fps = UInt32(max(30, screen?.maximumFramesPerSecond ?? 60))
        let id = lk_connect(target, UInt32(size.width * scale), UInt32(size.height * scale), fps)
        sessions[id] = SessionModel(id: id, target: target)
        return id
    }

    func session(_ id: UInt64) -> SessionModel? {
        sessions[id]
    }

    func submitPin(_ pin: String, for id: UInt64) {
        sessions[id]?.phase = .connecting
        lk_submit_pin(id, pin)
    }

    func disconnect(_ id: UInt64) {
        lk_disconnect(id)
        sessions[id] = nil
    }

    func stats(for id: UInt64) -> SessionStats? {
        decode(SessionStats.self, lk_session_stats(id))
    }

    /// Switches a session between viewing and controlling. Control waits for the host to grant it.
    /// `takeOver` takes control from another Mac that has it. The choice is remembered for that
    /// Mac (`remember`) and used on the next connection.
    func setControl(_ on: Bool, for id: UInt64, takeOver: Bool = false, remember: Bool = true) {
        guard let session = sessions[id] else { return }
        session.mode = on ? .control : .view
        session.control = on ? .requesting : .off
        if !on {
            session.cursor.reset()
            session.showToast("View only")
        }
        if remember, !session.hostId.isEmpty {
            UserDefaults.standard.set(on ? "control" : "view", forKey: Self.modeKey(session.hostId))
        }
        session.latestRequest = lk_set_control(id, on, takeOver)
    }

    private static func modeKey(_ hostId: String) -> String { "mode.\(hostId)" }

    /// Before quitting: releases keys held on remote Macs and for remote viewers.
    func shutdown() {
        guard thisMac != nil else { return }
        lk_shutdown()
    }

    // MARK: Host actions

    func kick(_ viewer: Viewer) { lk_kick_viewer(viewer.id) }

    /// Takes control back from a viewer; it keeps viewing.
    func stopControl(_ viewer: Viewer) { lk_stop_control(viewer.id) }

    func stopAllControl() {
        lk_stop_all_control()
    }

    func setAllowControl(_ allow: Bool) {
        lk_set_allow_control(allow)
        refreshHost()
    }

    // MARK: Accessibility (needed to be controlled)

    /// Asks macOS to let LanKVM post input: shows the system prompt the first time (which adds
    /// LanKVM to the list), then opens the settings pane.
    func requestControlPermission() {
        let options = [kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String: true] as CFDictionary
        if !AXIsProcessTrustedWithOptions(options) {
            openAccessibilitySettings()
        }
        refreshHost()
    }

    func openAccessibilitySettings() {
        let url = URL(string: "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")!
        NSWorkspace.shared.open(url)
    }

    func deny(_ request: PairingRequest) { lk_deny_pairing(request.id) }

    func forget(_ device: PairedDevice, canControlThisMac: Bool) {
        lk_forget_device(canControlThisMac ? "viewer" : "host", device.fingerprint)
    }

    // MARK: Screen Recording permission

    func openScreenRecordingSettings() {
        let url = URL(string: "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture")!
        NSWorkspace.shared.open(url)
    }

    /// Selects LanKVM.app in Finder, ready to drag into the Screen Recording list.
    func revealAppInFinder() {
        NSWorkspace.shared.activateFileViewerSelecting([Bundle.main.bundleURL])
    }

    /// macOS applies a new Screen Recording grant only after the app restarts.
    func relaunch() {
        let path = Bundle.main.bundlePath
        // A second copy on this Mac (scripts/second-instance.sh) keeps its own port and data.
        let env = ProcessInfo.processInfo.environment
        let keep = ["LANKVM_PORT", "LANKVM_DATA_DIR", "LANKVM_INJECT"].compactMap { key in env[key].map { "\(key)=\($0)" } }
        let task = Process()
        task.executableURL = URL(fileURLWithPath: "/bin/sh")
        // Wait for this instance to exit so the new one can take over the network port.
        task.arguments = ["-c", "sleep 1; /usr/bin/open -n \"$0\" \"$@\"", path] + keep.flatMap { ["--env", $0] }
        try? task.run()
        NSApp.terminate(nil)
    }

    // MARK: Helpers

    private func takeString(_ ptr: UnsafeMutablePointer<CChar>?) -> String? {
        guard let ptr else { return nil }
        defer { lk_string_free(ptr) }
        return String(cString: ptr)
    }

    private func decode<T: Decodable>(_ type: T.Type, _ ptr: UnsafeMutablePointer<CChar>?) -> T? {
        guard let text = takeString(ptr) else { return nil }
        return try? JSONDecoder().decode(type, from: Data(text.utf8))
    }
}

/// State of one viewer window.
@MainActor
final class SessionModel: ObservableObject, Identifiable {
    enum Phase: Equatable {
        case connecting
        case needsPin
        case connected(SessionInfo)
        case ended(String?)
    }

    /// What the user picked: only look at the remote Mac, or use it.
    enum Mode { case view, control }

    enum ControlStatus: Equatable {
        case off
        /// Asked the host; waiting for its answer.
        case requesting
        case active
        /// The host said no, or took control back: a `ControlReason` code and the host's words.
        case refused(Int, String)
    }

    let id: UInt64
    let target: String
    @Published var phase: Phase = .connecting
    @Published var mode: Mode = .view
    @Published var control: ControlStatus = .off
    /// A short message shown briefly over the screen.
    @Published private(set) var toast: String?
    var hostId = ""
    /// The host stamps this on events it injects; same-Mac viewers drop them (no feedback loop).
    var injectedTag: Int64 = 0
    /// The host's process id, a second way to recognise its events on the same Mac.
    var hostPid: Int64 = 0
    /// The host is this same Mac (testing): pointer and keyboard are shared.
    var sameMachine = false
    /// Id of the latest control request; answers to older ones are ignored.
    var latestRequest: UInt32 = 0
    let cursor = RemoteCursor()
    /// The floating control over the remote screen (and the Control menu's actions).
    private(set) lazy var sessionControl = SessionControlModel(session: self)
    /// Whether the session ever showed the remote screen (for wording when it ends).
    var wasConnected = false

    init(id: UInt64, target: String) {
        self.id = id
        self.target = target
    }

    var isControlling: Bool { control == .active }

    var hostName: String {
        if case .connected(let info) = phase { return info.hostName }
        return target
    }

    private var toastGeneration = 0

    /// Also read out by VoiceOver: each says what just changed.
    func showToast(_ text: String) {
        toast = text
        AccessibilityNotification.Announcement(text).post()
        toastGeneration += 1
        let generation = toastGeneration
        DispatchQueue.main.asyncAfter(deadline: .now() + 2.2) { [weak self] in
            MainActor.assumeIsolated {
                if self?.toastGeneration == generation { self?.toast = nil }
            }
        }
    }
}

/// The remote Mac's cursor, drawn locally while controlling it so the pointer moves without delay.
@MainActor
final class RemoteCursor {
    enum State: Equatable {
        case shape(UInt32)
        case hidden
        /// The host can't report its cursor; it's in the video, so draw only a small dot here.
        case inVideo
    }

    var state: State = .inVideo {
        didSet {
            if case .shape(let id) = state { lastShape = id }
            if state != oldValue { onChange?() }
        }
    }
    private var lastShape: UInt32?
    /// Called when the cursor to show changes.
    var onChange: (() -> Void)?
    private var shapes: [UInt32: NSCursor] = [:]

    func add(id: UInt32, png: Data, width: Double, height: Double, hotX: Double, hotY: Double) {
        guard let image = NSImage(data: png), width > 0, height > 0 else { return }
        image.size = NSSize(width: width, height: height)
        shapes[id] = NSCursor(image: image, hotSpot: NSPoint(x: hotX, y: hotY))
        if state == .shape(id) { onChange?() }
    }

    /// The host reports only changes: an unhide here that it doesn't confirm soon is undone.
    private var unhidePending = false
    /// One unhide per hide the host reports.
    private var mayUnhide = false
    private var unhideGeneration = 0

    func reset() {
        cancelUnhide()
        state = .inVideo
    }

    /// What the host says its cursor is now.
    func hostReported(_ new: State) {
        cancelUnhide()
        mayUnhide = new == .hidden
        state = new
    }

    /// The host hides its cursor while someone types there and shows it again on the next mouse
    /// move, which our move causes: show the last shape right away. If an app there keeps it
    /// hidden (a game, an app drawing its own), the host stays silent: hide it again then.
    func unhide() {
        guard state == .hidden, mayUnhide, let last = lastShape else { return }
        mayUnhide = false
        unhidePending = true
        unhideGeneration += 1
        let generation = unhideGeneration
        state = .shape(last)
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.3) { [weak self] in
            MainActor.assumeIsolated {
                guard let self, self.unhidePending, self.unhideGeneration == generation else { return }
                self.unhidePending = false
                self.state = .hidden
            }
        }
    }

    private func cancelUnhide() {
        unhidePending = false
        mayUnhide = false
    }

    /// Forgets every shape (the host numbers them again after a new grant).
    func clear() {
        cancelUnhide()
        shapes.removeAll()
        lastShape = nil
        state = .inVideo
    }

    var current: NSCursor {
        switch state {
        case .shape(let id): shapes[id] ?? .arrow
        case .hidden: Self.invisible
        case .inVideo: Self.dot
        }
    }

    static let invisible: NSCursor = {
        NSCursor(image: NSImage(size: NSSize(width: 1, height: 1)), hotSpot: .zero)
    }()

    /// A small ring: where the pointer is, while the real cursor arrives with the video.
    static let dot: NSCursor = {
        let size = NSSize(width: 10, height: 10)
        let image = NSImage(size: size, flipped: false) { rect in
            let ring = NSBezierPath(ovalIn: rect.insetBy(dx: 1.5, dy: 1.5))
            NSColor.white.withAlphaComponent(0.9).setFill()
            ring.fill()
            NSColor.black.withAlphaComponent(0.7).setStroke()
            ring.lineWidth = 1
            ring.stroke()
            return true
        }
        return NSCursor(image: image, hotSpot: NSPoint(x: 5, y: 5))
    }()
}

/// Set while a host-status refresh is queued: changes in a burst need only one.
private let hostRefreshQueued = OSAllocatedUnfairLock(initialState: false)

/// Called by the core on arbitrary threads.
private func coreEventCallback(_ json: UnsafePointer<CChar>?, _ ctx: UnsafeMutableRawPointer?) {
    guard let json else { return }
    let data = Data(String(cString: json).utf8)
    guard let event = try? JSONDecoder().decode(CoreEvent.self, from: data) else { return }
    if event.type == .hostChanged {
        let alreadyQueued = hostRefreshQueued.withLock { queued in
            defer { queued = true }
            return queued
        }
        if alreadyQueued { return }
        DispatchQueue.main.async {
            hostRefreshQueued.withLock { $0 = false }
            MainActor.assumeIsolated { CoreModel.shared.handle(event) }
        }
        return
    }
    DispatchQueue.main.async {
        MainActor.assumeIsolated { CoreModel.shared.handle(event) }
    }
}
