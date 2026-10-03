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
    /// `displays`: two Macs added displays here, one of them still connected.
    func loadSampleState(screenAllowed: Bool, displays: Bool = false) {
        thisMac = ThisMac(name: "Alex's MacBook Pro", addresses: ["192.168.1.23", "10.0.0.7"], port: 47800, deviceId: "724e:b7c8:8a63:ada8")
        host = HostStatus(
            viewers: [Viewer(id: 1, name: "Studio", address: "192.168.1.31", deviceId: "9f12:0ab3:77c1:e402", controlling: screenAllowed,
                             displayId: displays ? 5 : 1, virtualDisplay: displays)],
            pairing: [],
            screenCaptureAllowed: screenAllowed,
            allowControl: true,
            controlPermission: screenAllowed,
            virtualDisplays: displays ? [
                VirtualDisplay(displayId: 5, owner: "Studio", width: 6144, height: 2560, hidpi: true, arrangement: .only, inUse: true),
                VirtualDisplay(displayId: 6, owner: "Mac mini", width: 3840, height: 2160, hidpi: true, arrangement: .extend, inUse: false),
            ] : []
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
        // Another user takes this Mac's screen (fast user switching): the displays made for other
        // Macs hold this user's windows, and must not stay on the screen with the next user.
        // While another user has the screen, viewers can't add one either (until this user is back).
        NSWorkspace.shared.notificationCenter.addObserver(
            forName: NSWorkspace.sessionDidResignActiveNotification, object: nil, queue: .main
        ) { _ in
            lk_set_console_active(false)
        }
        NSWorkspace.shared.notificationCenter.addObserver(
            forName: NSWorkspace.sessionDidBecomeActiveNotification, object: nil, queue: .main
        ) { _ in
            lk_set_console_active(true)
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
        case .pinNeeded, .connected, .ended, .control, .cursorShape, .cursor, .display, .streamError:
            guard let id = event.session, let session = sessions[id] else { return }
            // Each kind of a session's event has its own case: a kind missing here must not end
            // the session.
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
                    restoreDisplay(for: session, info: info)
                }
                refreshRecents()
            case .ended:
                session.phase = .ended(event.error)
                session.control = .off
                session.mode = .view
                session.display = .idle
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
            case .display:
                // Only what the session shows changes here: none of `connected`'s side effects run
                // again, and an ended session stays ended.
                guard let info = event.info, case .connected(let before) = session.phase else { return }
                session.phase = .connected(info)
                displayChanged(session, from: before.display, to: info, request: event.request ?? 0,
                               reason: event.reason ?? DisplayReason.none, message: event.message ?? "")
            case .streamError:
                guard case .connected = session.phase else { return }
                session.streamError = event.message ?? "This Mac can't show the picture “\(session.hostName)” sends."
                session.streamErrorSize = event.width.flatMap { w in event.height.map { (UInt32(w), UInt32($0)) } }
            case .hostChanged, .trustChanged:
                break
            }
        }
    }

    func refreshHost() {
        if let status = decode(HostStatus.self, lk_host_status()), status != host {
            host = status
            // ⌃⌥⌘. takes control back and removes the displays made for other Macs, but only claims
            // the shortcut while someone controls this Mac or such a display exists.
            StopControlHotKey.shared.setEnabled(status.controller != nil || !status.virtualDisplays.isEmpty) { [weak self] in
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

    // MARK: Displays

    /// The host gets this long to show a display asked for; then the session stops waiting.
    private static let displayTimeout: TimeInterval = 15

    /// Asks a session's host to show `display`: its own screen (kind main), or a display it makes
    /// for this Mac (virtual). The session shows the new picture once the host answers; meanwhile
    /// it's switching, and the pointer rests. `matchedScreen`: the size of this Mac's screen it was
    /// picked to fill, remembered with it. `fromConnect`: put back on connecting, not the user's
    /// choice (it's already remembered).
    func showDisplay(_ display: RemoteDisplay, for id: UInt64, matchedScreen: CGSize? = nil, fromConnect: Bool = false) {
        guard let session = sessions[id], case .connected(let info) = session.phase else { return }
        // The user wants the host's own screen: not a display next time either (even when it
        // shows that already, a display offered or refused on connecting).
        if display.kind == .main, !fromConnect { RememberedDisplay.forget(session.hostId) }
        // Asking for what's shown, or about to be, changes nothing. A request the window stopped
        // waiting for is still answered later, and that answer applies: any pick then asks again,
        // so its answer comes last and the late one only brings the picture.
        let outstanding = session.requestedDisplay?.display
        let target: RemoteDisplay? = session.display.isSwitching ? outstanding : (outstanding == nil ? info.display : nil)
        if let target, display.matches(target) {
            if !session.display.isSwitching { session.display = .idle }
            return
        }
        // A new picture: what this Mac couldn't show was the old one.
        session.streamError = nil
        let request = switch display.kind {
        case .main: lk_show_main_display(id)
        case .virtual: lk_show_virtual_display(id, display.width, display.height, display.hidpi, display.refreshHz,
                                               display.arrangement.code)
        }
        session.latestDisplayRequest = request
        session.requestedDisplay = SessionModel.RequestedDisplay(display: display, matchedScreen: matchedScreen,
                                                                 remember: !fromConnect)
        session.display = .switching(request: request, label: display.kind == .main ? "its own screen" : display.sizeText,
                                     fromConnect: fromConnect)
        DispatchQueue.main.asyncAfter(deadline: .now() + Self.displayTimeout) { [weak session] in
            MainActor.assumeIsolated {
                guard let session, session.latestDisplayRequest == request, session.display.isSwitching else { return }
                // A late answer still applies (`displayChanged`); the window stops waiting for it.
                session.display = .failed(DisplayReason.failed, "“\(session.hostName)” didn't answer.", retry: display)
            }
        }
    }

    /// A `display` event: the session shows `info` now. Ends the switch it answers, and says how it
    /// went; otherwise it's the host's own change, with news to tell or a display taken away.
    private func displayChanged(_ session: SessionModel, from before: RemoteDisplay, to info: SessionInfo, request: UInt32,
                                reason: Int, message: String) {
        let host = "“\(info.hostName)”"
        // A stream error about a picture of another size is about the picture before this one.
        if session.streamError != nil, let size = session.streamErrorSize, size != (info.width, info.height) {
            session.streamError = nil
        }
        guard request != 0, request == session.latestDisplayRequest else {
            // Answers to replaced requests only bring the picture (applied above).
            guard request == 0 else { return }
            if reason == DisplayReason.none {
                if !message.isEmpty { session.showToast(message) }
            } else if session.display.isSwitching {
                // The answer to the user's request comes next; this is only news meanwhile.
                if !message.isEmpty { session.showToast(message) }
            } else {
                session.display = .failed(reason, message.isEmpty ? Self.displayProblem(reason, host: host) : message,
                                          retry: before.kind == .virtual ? before : RememberedDisplay.load(session.hostId)?.display)
            }
            return
        }
        let asked = session.requestedDisplay
        session.requestedDisplay = nil
        guard reason == DisplayReason.none else {
            // The host can't make displays: don't ask again on every connection.
            if reason == DisplayReason.unsupported { RememberedDisplay.forget(session.hostId) }
            session.display = .failed(reason, message.isEmpty ? Self.displayProblem(reason, host: host) : message,
                                      retry: asked?.display)
            return
        }
        session.display = .idle
        // Shown, but this Mac can't decode it: not worth keeping (the banner offers a way back).
        guard session.streamError == nil else { return }
        if let asked, asked.remember, asked.display.kind == .virtual {
            RememberedDisplay(display: asked.display, matchedScreen: asked.matchedScreen).save(session.hostId)
        }
        if info.display.kind == .main {
            session.showToast(message.isEmpty ? "\(host) shows its own screen again" : message)
            return
        }
        // The host's words when it has some (it changed something else to make room).
        guard message.isEmpty else { return session.showToast(message) }
        var text = "\(host) now uses \(info.display.sizeText)"
        if info.display.hidpi { text += " · looks like \(info.display.looksLikeText)" }
        // Fitted to this window's screen: in full screen it's shown pixel for pixel.
        // (Put back on connecting, the window shows no picture yet, so its screen isn't known:
        // the one in front.)
        let controls = session.sessionControl
        if let fit = controls.thisScreen ?? ScreenFit.main(), asked?.matchedScreen == fit.size, !controls.isFullScreen {
            // While controlling, ⌃⌘F goes to the remote Mac: release first.
            text += controls.isForwarding ? " · ⌃⌥⌘, then ⌃⌘F, shows it full screen pixel for pixel"
                : " · Enter Full Screen (⌃⌘F) to see it pixel for pixel"
        }
        session.showToast(text)
    }

    /// On connecting: the display last used with this host, unless it shows that already (it keeps
    /// a display a minute for a Mac whose connection was lost). Asked for right away, so the
    /// window never shows the host's own screen in between. One fitted to a screen of this Mac
    /// that isn't attached now is only offered.
    private func restoreDisplay(for session: SessionModel, info: SessionInfo) {
        guard let remembered = RememberedDisplay.load(info.hostId), !info.display.matches(remembered.display) else { return }
        if let screen = remembered.matchedScreen, !ScreenFit.all().contains(where: { $0.size == screen }) {
            session.display = .offered(remembered.display)
            return
        }
        showDisplay(remembered.display, for: session.id, matchedScreen: remembered.matchedScreen, fromConnect: true)
    }

    /// What to say when the host gave a reason without words.
    private static func displayProblem(_ reason: Int, host: String) -> String {
        switch reason {
        case DisplayReason.invalid: "\(host) can't make a display of that size."
        case DisplayReason.notAllowed: "\(host) lets this Mac only view it, so it can't add a display for it."
        case DisplayReason.unsupported: "\(host) can't add displays: its macOS doesn't support them."
        case DisplayReason.removedByHost: "Someone on \(host) removed the display made for this Mac."
        case DisplayReason.gone: "The display made for this Mac on \(host) is gone."
        case DisplayReason.sameMac: "On this same Mac, a display can only go next to its own screens."
        case DisplayReason.inUse: "Another Mac controls \(host), so a display here can only go next to its screens."
        case DisplayReason.tooMany: "\(host) already has as many displays for other Macs as it can make."
        default: "\(host) couldn't add the display. Try again in a moment."
        }
    }

    /// Before quitting: releases keys held on remote Macs and for remote viewers.
    func shutdown() {
        guard thisMac != nil else { return }
        lk_shutdown()
    }

    // MARK: Host actions

    func kick(_ viewer: Viewer) { lk_kick_viewer(viewer.id) }

    /// Takes control back from a viewer; it keeps viewing.
    func stopControl(_ viewer: Viewer) { lk_stop_control(viewer.id) }

    /// Takes control back from everyone, and removes the displays made for other Macs (⌃⌥⌘.).
    func stopAllControl() {
        lk_stop_all_control()
    }

    /// Removes a display made for another Mac (nil: all of them). Its viewer goes back to this
    /// Mac's own screen; the host status follows.
    func removeVirtualDisplay(_ display: VirtualDisplay?) {
        lk_remove_virtual_display(display?.displayId ?? 0)
    }

    func setAllowControl(_ allow: Bool) {
        lk_set_allow_control(allow)
        refreshHost()
    }

    // MARK: Accessibility (needed to be controlled, and to send Dock gestures)

    /// Asks macOS to let LanKVM post and filter input: shows the system prompt the first time
    /// (which adds LanKVM to the list), then opens the settings pane.
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

    /// Where a change of the display the session shows is at. What it shows is in its info.
    enum DisplayStatus: Equatable {
        case idle
        /// Asked the host for another display ("6144 × 2560", "its own screen") and waiting for
        /// its picture. `fromConnect`: put back on connecting; the window shows no picture until
        /// it's there.
        case switching(request: UInt32, label: String, fromConnect: Bool)
        /// The host didn't show what was asked for, or took a display away: a `DisplayReason`
        /// code, its words, and what asking again would ask for (nil: asking again won't help).
        case failed(Int, String, retry: RemoteDisplay?)
        /// Remembered for this host, but fitted to a screen of this Mac that isn't attached now:
        /// offered rather than asked for.
        case offered(RemoteDisplay)

        var isSwitching: Bool {
            if case .switching = self { return true }
            return false
        }
    }

    /// A display request waiting for its answer.
    struct RequestedDisplay {
        var display: RemoteDisplay
        /// The size of the screen of this Mac it was picked to fill, if it was.
        var matchedScreen: CGSize?
        /// The user's choice: remembered for this host once the host shows it.
        var remember: Bool
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
    /// Changing the display shown, or why it didn't change.
    @Published var display: DisplayStatus = .idle
    /// Id of the latest display request (they're numbered apart from control's): only its answer
    /// ends a switch.
    var latestDisplayRequest: UInt32 = 0
    /// What that request asked for, until it's answered.
    var requestedDisplay: RequestedDisplay?
    /// Why this Mac can't show the session's video (e.g. it can't decode that size), until the
    /// picture changes or the user closes the banner.
    @Published var streamError: String?
    /// The size of the picture it is about.
    var streamErrorSize: (UInt32, UInt32)?
    let cursor = RemoteCursor()
    /// The floating control over the remote screen (and the Control menu's actions).
    private(set) lazy var sessionControl = SessionControlModel(session: self)
    /// The user closed the hint about Accessibility for trackpad gestures: not again in this window.
    @Published var gestureHintDismissed = false
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
