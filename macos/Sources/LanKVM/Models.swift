import Foundation

// JSON shapes produced by the Rust core (crates/core). Keys are camelCase on both sides.

struct ThisMac: Decodable {
    var name: String
    var addresses: [String]
    var port: UInt16
    var deviceId: String
}

struct HostStatus: Decodable, Equatable {
    var viewers: [Viewer] = []
    var pairing: [PairingRequest] = []
    var screenCaptureAllowed: Bool = false
    /// Paired Macs may control this one (a setting).
    var allowControl: Bool = true
    /// macOS lets LanKVM post input (Accessibility).
    var controlPermission: Bool = false

    /// Who controls this Mac right now, if anyone.
    var controller: Viewer? { viewers.first(where: \.controlling) }
}

struct Viewer: Decodable, Identifiable, Equatable {
    var id: UInt64
    var name: String
    var address: String
    var deviceId: String
    var controlling: Bool = false
}

struct PairingRequest: Decodable, Identifiable, Equatable {
    var id: UInt64
    var name: String
    var address: String
    var pin: String
}

struct PairedDevice: Decodable, Identifiable, Equatable {
    var fingerprint: String
    var deviceId: String
    var name: String
    var id: String { fingerprint }
}

struct PairedDevices: Decodable, Equatable {
    var viewers: [PairedDevice] = []
    var hosts: [PairedDevice] = []
}

struct RecentHost: Decodable, Identifiable, Equatable {
    var address: String
    var name: String
    var id: String { address }
}

struct SessionInfo: Decodable, Equatable {
    var hostName: String
    var hostId: String = ""
    /// The host is this same Mac (a second copy of LanKVM, for testing).
    var sameMachine: Bool = false
    var address: String
    var width: UInt32
    var height: UInt32
    var fps: UInt32
    var codec: String
}

struct SessionStats: Decodable, Equatable {
    var fps: Double
    var mbps: Double
    var totalMs: Double?
    var captureMs: Double?
    var encodeMs: Double?
    var networkMs: Double?
    var decodeMs: Double?
    var displayMs: Double?
    var rttMs: Double?
    var framesShown: UInt64
    var framesLost: UInt64
    var keyframeRequests: UInt64
    /// This Mac's input → injected on the remote Mac, while controlling it.
    var inputMs: Double?
    var inputsSent: UInt64 = 0
}

/// Events pushed by the core (see `Event` in crates/core/src/lib.rs).
struct CoreEvent: Decodable {
    enum Kind: String, Decodable {
        case hostChanged, trustChanged, pinNeeded, connected, ended, control, cursorShape, cursor
    }
    var type: Kind
    var session: UInt64?
    var info: SessionInfo?
    var error: String?
    // control
    var request: UInt32?
    var active: Bool?
    /// `ControlReason` code (see crates/protocol): why control isn't active.
    var reason: Int?
    var message: String?
    var injectedTag: Int64?
    var hostPid: Int64?
    // cursorShape / cursor
    var id: UInt32?
    var png: Data?
    var width: Double?
    var height: Double?
    var hotX: Double?
    var hotY: Double?
    var state: String?
}

/// Why the host didn't grant control or took it back (`ControlReason` in crates/protocol).
enum ControlReason {
    static let none = 0
    static let turnedOff = 1
    static let needsPermission = 2
    static let inUse = 3
    static let takenOver = 4
    static let stoppedByHost = 5
    static let permissionLost = 6
    static let sameMac = 7
    static let selfConnection = 8
    static let badInput = 9
    static let displayGone = 10
    static let testTimeout = 11
}
