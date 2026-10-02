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
}

struct Viewer: Decodable, Identifiable, Equatable {
    var id: UInt64
    var name: String
    var address: String
    var deviceId: String
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
    var encodeMs: Double?
    var networkMs: Double?
    var decodeMs: Double?
    var displayMs: Double?
    var rttMs: Double?
    var framesShown: UInt64
    var framesLost: UInt64
    var keyframeRequests: UInt64
}

/// Events pushed by the core (see `Event` in crates/core/src/lib.rs).
struct CoreEvent: Decodable {
    enum Kind: String, Decodable {
        case hostChanged, trustChanged, pinNeeded, connected, ended
    }
    var type: Kind
    var session: UInt64?
    var info: SessionInfo?
    var error: String?
}
