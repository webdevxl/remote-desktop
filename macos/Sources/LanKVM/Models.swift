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
    /// Displays this Mac made for viewers, in use or kept a while for their viewer to come back.
    var virtualDisplays: [VirtualDisplay] = []
    /// Whether paired Macs can reach this one over the internet.
    var internet = InternetStatus()
    /// Sunshine, the other engine a viewer may stream this screen with.
    var sunshine = SunshineStatus()

    /// Who controls this Mac right now, if anyone.
    var controller: Viewer? { viewers.first(where: \.controlling) }
}

extension HostStatus {
    private enum CodingKeys: String, CodingKey {
        case viewers, pairing, screenCaptureAllowed, allowControl, controlPermission, virtualDisplays, internet, sunshine
    }

    /// Newer fields are optional: a status without them (or with one this app can't read) still
    /// shows who is connected.
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        viewers = try c.decode([Viewer].self, forKey: .viewers)
        pairing = try c.decode([PairingRequest].self, forKey: .pairing)
        screenCaptureAllowed = try c.decode(Bool.self, forKey: .screenCaptureAllowed)
        allowControl = try c.decode(Bool.self, forKey: .allowControl)
        controlPermission = try c.decode(Bool.self, forKey: .controlPermission)
        virtualDisplays = c.lenient([VirtualDisplay].self, .virtualDisplays) ?? []
        internet = c.lenient(InternetStatus.self, .internet) ?? InternetStatus()
        sunshine = c.lenient(SunshineStatus.self, .sunshine) ?? SunshineStatus()
    }
}

/// Sunshine on this Mac (`SunshineView` in crates/core sunshine.rs). LanKVM starts it for one
/// viewer at a time, while that viewer streams with Sunshine + Moonlight, and stops it after.
struct SunshineStatus: Equatable {
    var installed = false
    var running = false
    /// The name of the Mac it streams to.
    var viewer: String?
}

extension SunshineStatus: Decodable {
    private enum CodingKeys: String, CodingKey {
        case installed, running, viewer
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        installed = c.lenient(Bool.self, .installed) ?? false
        running = c.lenient(Bool.self, .running) ?? false
        viewer = c.lenient(String.self, .viewer).flatMap { $0.isEmpty ? nil : $0 }
    }
}

/// Internet access to this Mac (`InternetView` in crates/core): the setting, and how far asking
/// the router to forward LanKVM's port got.
struct InternetStatus: Equatable {
    enum State: String {
        case off
        /// Asking the router to forward the port.
        case requesting
        /// The router forwards it.
        case mapped
        /// This Mac's own address is public: nothing to forward.
        case `public`
        /// See `problem`. The core keeps asking in the background.
        case problem
    }

    enum Problem: String {
        /// The router didn't answer (no UPnP, NAT-PMP or PCP).
        case noResponse
        /// The router can't forward ports automatically.
        case unsupported
        /// Automatic port forwarding is turned off on the router.
        case disabled
        /// The router is behind another router.
        case doubleNat
        /// The internet provider shares one public address among many customers.
        case cgnat
        /// Not connected to a network with a router.
        case noRouter
        /// macOS's service that talks to the router isn't responding.
        case serviceDown
        /// The macOS Firewall blocks LanKVM.
        case firewall
        case other
    }

    /// The setting: paired Macs may connect over the internet.
    var enabled = false
    var state = State.off
    var problem: Problem?
    /// Where the internet reaches this Mac ("203.0.113.7:47800"), once the router forwards the
    /// port or when this Mac's address is public.
    var externalAddress: String?
    /// The router's own outside address, when that isn't public (doubleNat, cgnat).
    var routerAddress: String?
    /// This Mac's address on the local network, to forward the port to by hand.
    var localAddress: String?
    /// The UDP port LanKVM uses on this Mac.
    var port: UInt16 = 47800
    /// The address the user gave for other Macs to use (a dynamic DNS name or an IP), or "".
    var publicAddress = ""
    /// The addresses paired Macs are told to use over the internet.
    var announced: [String] = []
    /// Packets from the internet that didn't come from a paired Mac, dropped since LanKVM started
    /// (scanners, their retries, and Macs that were forgotten). Not a count of devices.
    var ignored: UInt64 = 0

    /// Paired Macs can reach this one: the router forwards the port, or nothing needs forwarding.
    var isOpen: Bool { enabled && (state == .mapped || state == .public) }

    /// Nothing on this Mac or its router can fix it: the network itself keeps the internet out.
    var isUnreachable: Bool {
        guard enabled, state == .problem else { return false }
        return problem == .cgnat || problem == .noRouter
    }

    /// LanKVM can't get the port opened, but forwarding it in the router's settings works (in
    /// both routers' for doubleNat).
    var canForwardByHand: Bool {
        switch problem ?? .other {
        case .noResponse, .unsupported, .disabled, .doubleNat, .other: true
        case .cgnat, .noRouter, .serviceDown, .firewall: false
        }
    }

    /// The router didn't open the port, but the user gave the address to use: they forwarded it
    /// themselves. The core can't tell whether that forward works.
    var isManual: Bool { enabled && state == .problem && canForwardByHand && !publicAddress.isEmpty }
}

extension InternetStatus: Decodable {
    private enum CodingKeys: String, CodingKey {
        case enabled, state, problem, externalAddress, routerAddress, localAddress, port, publicAddress, announced, ignored
    }

    /// Every field is optional. A state this app doesn't know is shown as a problem it can't name
    /// (forwarding the port by hand still helps), a problem it doesn't know likewise.
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        enabled = c.lenient(Bool.self, .enabled) ?? false
        let stateName = c.lenient(String.self, .state)
        state = stateName.flatMap(State.init) ?? (enabled && stateName != nil ? .problem : .off)
        let problemName = c.lenient(String.self, .problem)
        problem = problemName.map { Problem(rawValue: $0) ?? .other } ?? (state == .problem ? .other : nil)
        externalAddress = c.lenient(String.self, .externalAddress).flatMap { $0.isEmpty ? nil : $0 }
        routerAddress = c.lenient(String.self, .routerAddress).flatMap { $0.isEmpty ? nil : $0 }
        localAddress = c.lenient(String.self, .localAddress).flatMap { $0.isEmpty ? nil : $0 }
        port = c.lenient(UInt16.self, .port) ?? 47800
        publicAddress = c.lenient(String.self, .publicAddress) ?? ""
        announced = c.lenient([String].self, .announced) ?? []
        ignored = c.lenient(UInt64.self, .ignored) ?? 0
    }
}

struct Viewer: Decodable, Identifiable, Equatable {
    var id: UInt64
    var name: String
    var address: String
    var deviceId: String
    var controlling: Bool = false
    /// The display of this Mac it watches (CGDirectDisplayID).
    var displayId: UInt32 = 0
    /// It watches a display this Mac made for it, not this Mac's own screen.
    var virtualDisplay: Bool = false
    /// It connected over the internet, not from the local network.
    var internet: Bool = false
}

extension Viewer {
    private enum CodingKeys: String, CodingKey {
        case id, name, address, deviceId, controlling, displayId, virtualDisplay, internet
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(UInt64.self, forKey: .id)
        name = try c.decode(String.self, forKey: .name)
        address = try c.decode(String.self, forKey: .address)
        deviceId = try c.decode(String.self, forKey: .deviceId)
        controlling = c.lenient(Bool.self, .controlling) ?? false
        displayId = c.lenient(UInt32.self, .displayId) ?? 0
        virtualDisplay = c.lenient(Bool.self, .virtualDisplay) ?? false
        internet = c.lenient(Bool.self, .internet) ?? false
    }
}

/// A display this Mac made for a viewer (`VirtualDisplayView` in crates/core displays.rs).
struct VirtualDisplay: Identifiable, Equatable {
    var displayId: UInt32
    /// The name of the Mac it was made for.
    var owner: String
    /// Pixels.
    var width: UInt32
    var height: UInt32
    var hidpi = false
    var arrangement = RemoteDisplay.Arrangement.only
    /// Its viewer watches it now. Otherwise the viewer's connection was lost, and it's kept about
    /// a minute in case that Mac comes back.
    var inUse = true

    var id: UInt32 { displayId }

    /// As a viewer would see it (for its size and how big things look on it).
    var display: RemoteDisplay {
        RemoteDisplay(kind: .virtual, width: width, height: height, hidpi: hidpi, arrangement: arrangement)
    }
}

extension VirtualDisplay: Decodable {
    private enum CodingKeys: String, CodingKey {
        case displayId, owner, width, height, hidpi, arrangement, inUse
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        displayId = try c.decode(UInt32.self, forKey: .displayId)
        owner = c.lenient(String.self, .owner) ?? ""
        width = c.lenient(UInt32.self, .width) ?? 0
        height = c.lenient(UInt32.self, .height) ?? 0
        hidpi = c.lenient(Bool.self, .hidpi) ?? false
        arrangement = c.lenient(String.self, .arrangement).flatMap(RemoteDisplay.Arrangement.init) ?? .only
        inUse = c.lenient(Bool.self, .inUse) ?? true
    }
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
    /// A Mac this one controls: where it was last reached, or told this Mac to reach it, over the
    /// internet ("203.0.113.7:47800"). Nil when unknown.
    var internetAddress: String?
    var id: String { fingerprint }
}

extension PairedDevice {
    private enum CodingKeys: String, CodingKey {
        case fingerprint, deviceId, name, internetAddress
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        fingerprint = try c.decode(String.self, forKey: .fingerprint)
        deviceId = try c.decode(String.self, forKey: .deviceId)
        name = try c.decode(String.self, forKey: .name)
        internetAddress = c.lenient(String.self, .internetAddress).flatMap { $0.isEmpty ? nil : $0 }
    }
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
    /// The stream, in pixels: what the picture is now (a virtual display's size while one is shown).
    var width: UInt32
    var height: UInt32
    var fps: UInt32
    var codec: String
    /// The host display shown: its own screen, or one it made for this Mac.
    var display = RemoteDisplay()
    /// Whether this session may ask for a virtual display: `DisplayReason.none` if it may,
    /// `sameMac` if only one next to the host's screens, otherwise why not (in words in
    /// `displayUnavailable`, naming the host; "" while it may).
    var displayAvailable = DisplayReason.none
    var displayUnavailable = ""
    /// Connected over the internet, not on the local network.
    var internet = false
}

extension SessionInfo {
    private enum CodingKeys: String, CodingKey {
        case hostName, hostId, sameMachine, address, width, height, fps, codec, display, displayAvailable, displayUnavailable,
             internet
    }

    /// Lenient about everything but the stream itself: one field this app can't read would drop
    /// the whole `connected` event, and the window would say "Connecting…" forever.
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        hostName = try c.decode(String.self, forKey: .hostName)
        hostId = c.lenient(String.self, .hostId) ?? ""
        sameMachine = c.lenient(Bool.self, .sameMachine) ?? false
        address = c.lenient(String.self, .address) ?? ""
        width = try c.decode(UInt32.self, forKey: .width)
        height = try c.decode(UInt32.self, forKey: .height)
        fps = c.lenient(UInt32.self, .fps) ?? 60
        codec = c.lenient(String.self, .codec) ?? ""
        display = c.lenient(RemoteDisplay.self, .display)
            ?? RemoteDisplay(kind: .main, width: width, height: height, refreshHz: fps)
        displayAvailable = c.lenient(Int.self, .displayAvailable) ?? DisplayReason.none
        displayUnavailable = c.lenient(String.self, .displayUnavailable) ?? ""
        internet = c.lenient(Bool.self, .internet) ?? false
    }
}

/// The host display a session shows (`DisplayView` in crates/core client.rs): the host's own
/// screen, or a display the host made for this Mac that the session streams pixel for pixel.
struct RemoteDisplay: Equatable {
    enum Kind: String { case main, virtual }

    /// How a virtual display sits among the host's own ones.
    enum Arrangement: String, CaseIterable, Identifiable {
        /// The host's own displays mirror it, so every window is on it.
        case only
        /// The main display: the menu bar, the Dock and new windows go to it.
        case main
        /// Next to the host's own displays; windows stay where they are.
        case extend

        var id: Self { self }
    }

    var kind = Kind.main
    /// Pixels. For the host's own screen, the stream's size.
    var width: UInt32 = 0
    var height: UInt32 = 0
    /// Retina: drawn at 2x, so its desktop looks like half the size each way, with sharp text.
    var hidpi = false
    var refreshHz: UInt32 = 60
    /// Only for virtual displays.
    var arrangement = Arrangement.only

    /// Whether this is what `other` asks for: the host's own screen, or a display of the same
    /// size, sharpness, refresh rate and arrangement.
    func matches(_ other: RemoteDisplay) -> Bool {
        kind == other.kind && (kind == .main || self == other)
    }

    /// Whether it's the same size and sharpness as `other` (a checkmark in the Display menu).
    func sameSize(as other: RemoteDisplay) -> Bool {
        kind == .virtual && other.kind == .virtual && width == other.width && height == other.height && hidpi == other.hidpi
    }

    /// "6144 × 2560".
    var sizeText: String { Self.sizeText(width, height) }

    /// How big its desktop looks: half its pixels each way on Retina.
    var looksLikeText: String { hidpi ? Self.sizeText(width / 2, height / 2) : sizeText }

    static func sizeText(_ width: UInt32, _ height: UInt32) -> String { "\(width) × \(height)" }
}

extension RemoteDisplay: Decodable {
    private enum CodingKeys: String, CodingKey {
        case kind, width, height, hidpi, refreshHz, arrangement
    }

    /// A kind this app doesn't know is shown as the host's own screen, an arrangement it doesn't
    /// know as the default.
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        kind = c.lenient(String.self, .kind).flatMap(Kind.init) ?? .main
        width = c.lenient(UInt32.self, .width) ?? 0
        height = c.lenient(UInt32.self, .height) ?? 0
        hidpi = c.lenient(Bool.self, .hidpi) ?? false
        refreshHz = c.lenient(UInt32.self, .refreshHz) ?? 60
        arrangement = c.lenient(String.self, .arrangement).flatMap(Arrangement.init) ?? .only
    }
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
        case hostChanged, trustChanged, pinNeeded, connected, ended, control, cursorShape, cursor, display, streamError, engine, moonlight
    }
    var type: Kind
    var session: UInt64?
    /// connected, display
    var info: SessionInfo?
    var error: String?
    // control, display, engine
    var request: UInt32?
    var active: Bool?
    /// control: a `ControlReason` code (see crates/protocol), why control isn't active. display: a
    /// `DisplayReason` code, why the session doesn't show what it asked for.
    var reason: Int?
    /// control, display, engine: the reason in words, or news. streamError: why the video can't
    /// be shown. moonlight: how Moonlight is doing, in words.
    var message: String?
    var injectedTag: Int64?
    var hostPid: Int64?
    // cursorShape / cursor (streamError: the picture's size, in pixels)
    var id: UInt32?
    var png: Data?
    var width: Double?
    var height: Double?
    var hotX: Double?
    var hotY: Double?
    /// cursor: how to show it. moonlight: a `MoonlightState`.
    var state: String?
    /// engine: a `StreamEngine`, the one streaming now.
    var engine: String?
    /// engine: Sunshine's port while it streams.
    var port: UInt16?
    /// engine: the rest of what Moonlight connects with (a new generation is a new stream).
    var sunshine: SunshineStream?
    // moonlight: its process and log file.
    var pid: UInt32?
    var log: String?
}

/// Which engine streams a session's picture (`Engine` in crates/protocol): LanKVM's own, into
/// the viewer window, or the host's Sunshine, shown here by Moonlight in its own window.
enum StreamEngine: String {
    case lankvm, sunshine
}

/// How Moonlight is doing while a session streams with Sunshine (the `moonlight` event).
enum MoonlightState: String {
    /// Pairing with the host's Sunshine (LanKVM hands Sunshine the PIN Moonlight shows).
    case pairing
    case starting
    /// Its window shows the stream.
    case streaming
    /// It quit or was closed; it can be opened again.
    case ended
    /// It couldn't start or pair; the message says why. It can be tried again.
    case failed
}

/// The stream the host's Sunshine serves (`SunshineInfo` in crates/protocol).
struct SunshineStream: Equatable {
    var port: UInt16 = 0
    /// Bumped each time Sunshine starts again (another display): Moonlight reconnects.
    var generation: UInt32 = 0
    var width: UInt32 = 0
    var height: UInt32 = 0
    var fps: UInt32 = 0
}

extension SunshineStream: Decodable {
    private enum CodingKeys: String, CodingKey {
        case port, generation, width, height, fps
    }

    /// Lenient: it only adds detail to an `engine` event, which must never be dropped for it.
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        port = c.lenient(UInt16.self, .port) ?? 0
        generation = c.lenient(UInt32.self, .generation) ?? 0
        width = c.lenient(UInt32.self, .width) ?? 0
        height = c.lenient(UInt32.self, .height) ?? 0
        fps = c.lenient(UInt32.self, .fps) ?? 0
    }
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

/// Why a session doesn't show the display it asked for, or can't ask for a virtual display
/// (`DisplayReason` in crates/protocol, LK_DISPLAY_* in lankvm.h).
enum DisplayReason {
    static let none = 0
    static let invalid = 1
    /// The host lets paired Macs only view it.
    static let notAllowed = 2
    /// The host's macOS can't make virtual displays.
    static let unsupported = 3
    /// Trying again may work.
    static let failed = 4
    /// The host's user removed it.
    static let removedByHost = 5
    static let gone = 6
    /// The host is this same Mac: only a display next to its own.
    static let sameMac = 7
    /// Another device controls the host: no main or only display.
    static let inUse = 8
    static let noVideo = 9
    static let tooMany = 10
}

extension KeyedDecodingContainer {
    /// The value for `key`, or nil when it's missing or not what this app expects: a field the core
    /// added or changed must never drop the whole event or status.
    func lenient<T: Decodable>(_ type: T.Type, _ key: Key) -> T? {
        try? decodeIfPresent(type, forKey: key)
    }
}
