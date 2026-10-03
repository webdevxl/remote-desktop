import AppKit

/// Lets ⌘Tab, ⌘Space, Mission Control, screenshot keys and other system shortcuts reach the
/// remote Mac while controlling it, instead of acting on this one.
///
/// macOS handles those before any app sees them. While captured, LanKVM's connection to the
/// window server asks it to skip its global hot keys (except accessibility ones such as VoiceOver).
/// This is a private but long-standing call (SDL's keyboard grab, UTM and VirtualBox use it); it
/// needs no permission and only lasts while LanKVM is the active app with the connection open, so
/// a crash can't leave the Mac without ⌘Tab. LanKVM also turns it back off whenever the viewer
/// loses focus, and a watchdog does so if the main thread stalls.
@MainActor
enum SystemShortcuts {
    /// User setting: whether to send system shortcuts while controlling.
    static let defaultsKey = "sendSystemShortcuts"

    static var enabled: Bool {
        UserDefaults.standard.object(forKey: defaultsKey) as? Bool ?? true
    }

    private typealias MainConnection = @convention(c) () -> Int32
    private typealias SetMode = @convention(c) (Int32, UInt32) -> Int32
    private typealias GetMode = @convention(c) (Int32, UnsafeMutablePointer<UInt32>) -> Int32

    /// CGSGlobalHotKeyOperatingMode: 0 enabled, 2 disabled except Universal Access. 4 and 6
    /// (sleep, screen saver) belong to the system and are left alone.
    private static let enable: UInt32 = 0
    private static let disableExceptAccessibility: UInt32 = 2

    private struct API {
        let connection: MainConnection
        let set: SetMode
        let get: GetMode
    }

    private static let api: API? = {
        let handle = UnsafeMutableRawPointer(bitPattern: -2) // RTLD_DEFAULT
        guard let c = dlsym(handle, "CGSMainConnectionID"),
              let s = dlsym(handle, "CGSSetGlobalHotKeyOperatingMode"),
              let g = dlsym(handle, "CGSGetGlobalHotKeyOperatingMode") else { return nil }
        return API(connection: unsafeBitCast(c, to: MainConnection.self),
                   set: unsafeBitCast(s, to: SetMode.self),
                   get: unsafeBitCast(g, to: GetMode.self))
    }()

    private static var owners = Set<ObjectIdentifier>()
    private static var watchdog: DispatchSourceTimer?
    private static var heartbeat: Timer?
    /// Updated by the main thread; read by the watchdog.
    nonisolated(unsafe) private static var lastTick = ProcessInfo.processInfo.systemUptime

    /// Captures system shortcuts while any owner wants them.
    static func capture(_ on: Bool, owner: AnyObject) {
        let id = ObjectIdentifier(owner)
        if on { owners.insert(id) } else { owners.remove(id) }
        apply(!owners.isEmpty)
    }

    /// Gives system shortcuts back no matter who asked for them (app quitting).
    static func restore() {
        owners.removeAll()
        apply(false)
    }

    private static func apply(_ captured: Bool) {
        guard let api else { return }
        let cid = api.connection()
        var current: UInt32 = 0
        _ = api.get(cid, &current)
        guard current == enable || current == disableExceptAccessibility else { return }
        let wanted = captured ? disableExceptAccessibility : enable
        if current != wanted {
            _ = api.set(cid, wanted)
        }
        captured ? startWatchdog() : stopWatchdog()
    }

    /// If the main thread stops responding while shortcuts are captured, give them back to the
    /// user (⌘Tab, Force Quit...) from a background thread.
    private static func startWatchdog() {
        guard watchdog == nil, let api else { return }
        lastTick = ProcessInfo.processInfo.systemUptime
        let tick = Timer(timeInterval: 0.25, repeats: true) { _ in
            SystemShortcuts.lastTick = ProcessInfo.processInfo.systemUptime
        }
        RunLoop.main.add(tick, forMode: .common)
        heartbeat = tick
        let timer = DispatchSource.makeTimerSource(queue: .global(qos: .userInteractive))
        timer.schedule(deadline: .now() + 0.5, repeating: 0.5)
        let (connection, set) = (api.connection, api.set)
        timer.setEventHandler {
            if ProcessInfo.processInfo.systemUptime - SystemShortcuts.lastTick > 2 {
                _ = set(connection(), 0)
            }
        }
        timer.resume()
        watchdog = timer
    }

    private static func stopWatchdog() {
        watchdog?.cancel()
        watchdog = nil
        heartbeat?.invalidate()
        heartbeat = nil
    }
}
