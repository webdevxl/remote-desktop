import AppKit
import CLanKVM
import Combine
import Foundation

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
            viewers: [Viewer(id: 1, name: "Studio", address: "192.168.1.31", deviceId: "9f12:0ab3:77c1:e402")],
            pairing: [],
            screenCaptureAllowed: screenAllowed
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
        if !lk_screen_capture_allowed() {
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
        case .pinNeeded, .connected, .ended:
            guard let id = event.session, let session = sessions[id] else { return }
            switch event.type {
            case .pinNeeded: session.phase = .needsPin
            case .connected:
                if let info = event.info {
                    session.phase = .connected(info)
                    session.wasConnected = true
                }
                refreshRecents()
            default:
                session.phase = .ended(event.error)
            }
        }
    }

    func refreshHost() {
        if let status = decode(HostStatus.self, lk_host_status()), status != host {
            host = status
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
        let id = lk_connect(target, UInt32(size.width * scale), UInt32(size.height * scale))
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

    // MARK: Host actions

    func kick(_ viewer: Viewer) { lk_kick_viewer(viewer.id) }

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
        let task = Process()
        task.executableURL = URL(fileURLWithPath: "/bin/sh")
        // Wait for this instance to exit so the new one can take over the network port.
        task.arguments = ["-c", "sleep 1; /usr/bin/open \"$0\"", path]
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

    let id: UInt64
    let target: String
    @Published var phase: Phase = .connecting
    /// Whether the session ever showed the remote screen (for wording when it ends).
    var wasConnected = false

    init(id: UInt64, target: String) {
        self.id = id
        self.target = target
    }
}

/// Called by the core on arbitrary threads.
private func coreEventCallback(_ json: UnsafePointer<CChar>?, _ ctx: UnsafeMutableRawPointer?) {
    guard let json else { return }
    let data = Data(String(cString: json).utf8)
    guard let event = try? JSONDecoder().decode(CoreEvent.self, from: data) else { return }
    DispatchQueue.main.async {
        MainActor.assumeIsolated { CoreModel.shared.handle(event) }
    }
}
