import AppKit
import ApplicationServices
import os

/// Sends the trackpad's Dock gestures to the remote Mac while a viewer window forwards input:
/// swiping between Spaces, Mission Control, App Exposé, and the pinches for Show Desktop and
/// Launchpad (Apps).
///
/// AppKit never sees these; the Dock takes them from the window server. An active event tap sees
/// them first and can drop them. That needs Accessibility (as being controlled does), which LanKVM
/// never asks for by itself: `GestureAccessHint` offers it. Whether a gesture is taken is decided
/// when it begins and holds until it ends (`DockGestureClaim`), so the Dock never sees half of one.
///
/// The tap exists only while a window forwards with Send Trackpad Gestures on, and runs on its own
/// thread: every trackpad gesture on this Mac waits for its callback, so the callback never waits
/// for the main thread, and while the main thread stalls, gestures stay on this Mac. It dies with
/// the process: nothing outlives LanKVM.
@MainActor
enum DockGestures {
    /// The window taking Dock gestures now.
    private static weak var owner: InputForwarder?
    /// Bumped each time a window starts taking them: events of a gesture taken for an earlier one
    /// go nowhere.
    private static var generation = 0
    private static var timer: Timer?
    private static var ticks = 0
    /// Whether the tap's thread was ever needed.
    private static var tapUsed = false

    /// How often the main thread says it's alive; every `healthEvery` ticks the tap is checked.
    private static let tickInterval: TimeInterval = 0.5
    private static let healthEvery = 10

    /// Takes Dock gestures that begin over `area` (global display coordinates, top-left origin) for
    /// `owner`, which gets them through `dockSwipe(_:)`; called again, follows `area`. `injectedTag`
    /// is the tag `owner`'s remote Mac puts on events it injects. Does nothing without Accessibility.
    static func arm(_ owner: InputForwarder, area: CGRect, injectedTag: Int64) {
        let fresh = owner !== self.owner
        if fresh {
            // Without it the tap couldn't drop events, and creating one would make macOS ask.
            guard AXIsProcessTrusted() else { return disarmAll() }
            self.owner = owner
            generation += 1
            startTimer()
        }
        let arm = DockGestureClaim.Arm(generation: generation, area: area, injectedTag: injectedTag)
        let now = ProcessInfo.processInfo.systemUptime
        tapState.withLock { state in
            state.arm = arm
            state.wanted = true
            state.lastTick = now
        }
        if fresh {
            tapUsed = true
            GestureTap.sync()
        }
    }

    /// The picture of `owner` moved: Dock gestures count over its new place.
    static func update(area: CGRect, for owner: InputForwarder) {
        guard owner === self.owner else { return }
        tapState.withLock { $0.arm?.area = area }
    }

    /// Stops taking Dock gestures for `owner`. One already taken stays away from this Mac's Dock
    /// until it ends, but goes nowhere: `owner` ends it on the remote Mac (or, when forwarding
    /// stopped, its host did as it let go of everything).
    static func disarm(_ owner: InputForwarder) {
        guard owner === self.owner else { return }
        disarmAll()
    }

    private static func disarmAll(immediately: Bool = false) {
        // Still forwarding (the setting went off, Accessibility was revoked): nothing more of a Dock
        // swipe in progress will come, and heartbeats would keep it going there.
        owner?.cancelDockSwipe()
        owner = nil
        timer?.invalidate()
        timer = nil
        guard tapUsed else { return }
        tapState.withLock { state in
            state.arm = nil
            state.wanted = false
        }
        GestureTap.sync(immediately: immediately)
    }

    private static func startTimer() {
        timer?.invalidate()
        ticks = 0
        let timer = Timer(timeInterval: tickInterval, repeats: true) { _ in
            MainActor.assumeIsolated { tick() }
        }
        RunLoop.main.add(timer, forMode: .common)
        self.timer = timer
    }

    /// Shows the tap that the main thread is alive (it takes gestures only then), and every few
    /// seconds checks that the tap still may and does work.
    private static func tick() {
        // Its window went away without saying so.
        guard owner != nil else { return disarmAll() }
        let now = ProcessInfo.processInfo.systemUptime
        tapState.withLock { $0.lastTick = now }
        ticks += 1
        guard ticks % healthEvery == 0 else { return }
        // Accessibility was turned off: the tap must go at once (macOS may stop delivering clicks
        // while an active tap runs without it).
        guard AXIsProcessTrusted() else { return disarmAll(immediately: true) }
        GestureTap.revive()
    }

    /// From the tap: an event of a gesture taken for the window of `generation`.
    fileprivate static func received(_ sample: DockSwipeSample, for generation: Int) {
        guard generation == self.generation, let owner else { return }
        owner.dockSwipe(sample)
    }

    /// From the tap: it lost track of the gesture it was taking for the window of `generation`.
    fileprivate static func lost(_ generation: Int) {
        guard generation == self.generation, let owner else { return }
        owner.cancelDockSwipe()
    }
}

/// One Dock gesture event as this Mac's trackpad reported it: the raw field values, which the core
/// converts for the wire (`lk_input_dock_swipe`).
struct DockSwipeSample: Equatable, Sendable {
    /// LK_DOCK_*: the gesture's motion (horizontal, vertical, pinch).
    var axis: UInt8
    /// LK_PHASE_*.
    var phase: UInt8
    var progress: Double
    var velocityX: Double
    var velocityY: Double
    var inverted: Bool
    /// How many LanKVM hosts it passed through (0: this Mac's trackpad).
    var depth: UInt8
}

/// Which Dock gestures the tap takes from this Mac's Dock, and where their events go. Decided when
/// a gesture begins and kept until it ends: the Dock must see all of a gesture or none of it. Pure
/// (no tap, clock or threads), so the rules can be followed one event at a time.
struct DockGestureClaim: Sendable {
    /// A window taking Dock gestures.
    struct Arm: Equatable, Sendable {
        /// Changes each time a window starts taking them.
        var generation: Int
        /// Where the pointer must be when a gesture begins, in global display coordinates (points,
        /// top-left origin of the main display): the remote picture, or its whole screen in full
        /// screen.
        var area: CGRect
        /// The tag the window's remote Mac puts on events it injects (`SessionModel.injectedTag`).
        var injectedTag: Int64
    }

    /// Who made an event.
    enum Source: Equatable {
        /// This Mac's trackpad.
        case trackpad
        /// LanKVM's host on this Mac, for a Mac controlling this one: passed on, one host further.
        case relayed(depth: UInt8)
        /// Another app's synthetic gesture, or the window's own remote Mac when that is this Mac:
        /// the Dock here acts on it.
        case other
    }

    enum Verdict: Equatable {
        case pass
        case drop
        /// Pass it with no progress or velocity: the end of a gesture the Dock never saw move.
        /// macOS 27 needs it to close its gesture state (Space Rabbit).
        case passStill
    }

    /// What to do with one event.
    struct Step: Equatable {
        var verdict = Verdict.pass
        /// Send the event to the window of this generation...
        var send: Int?
        /// ...as relayed through this many LanKVM hosts.
        var depth: UInt8 = 0
        /// The gesture before never ended (its end was missed): cancel it in the window of this
        /// generation first.
        var cancel: Int?
    }

    /// The phase field's values (IOHIDEventPhaseBits, as LK_PHASE_*).
    static let began: Int64 = 1
    static let changed: Int64 = 2
    static let ended: Int64 = 4
    static let cancelled: Int64 = 8

    /// Taking the gesture in progress: its events don't reach this Mac's Dock until it ends.
    private(set) var claimed = false
    /// The generation its events go to; nil once that window stopped taking gestures.
    private(set) var target: Int?
    private var depth: UInt8 = 0

    /// Who made an event, from its source process (field 41) and user data (field 42).
    static func source(processId: Int64, userData: Int64, injectedTag: Int64) -> Source {
        if userData >> 32 == InputForwarder.injectedTagPrefix {
            if injectedTag != 0 && userData & ~InputForwarder.relayDepthMask == injectedTag { return .other }
            return .relayed(depth: UInt8(clamping: ((userData & InputForwarder.relayDepthMask) >> 24) + 1))
        }
        // The trackpad's events come from the window server, not from a process.
        return processId == 0 ? .trackpad : .other
    }

    /// A Dock swipe event (CGS type 30, subtype 23) in `phase`. At Began, `arm` (who takes gestures
    /// now) and `pointer` (where it is, global display coordinates) decide; later events follow
    /// that decision, and go to the same window as long as `arm` is still that window's.
    /// `stillEnd`: the Dock gets a taken gesture's Ended without its motion (macOS 27) instead of
    /// nothing.
    mutating func dockSwipe(phase: Int64, processId: Int64, userData: Int64, arm: Arm?, pointer: CGPoint?,
                            stillEnd: Bool) -> Step {
        var step = Step()
        if phase == Self.began {
            if claimed { step.cancel = target }
            claimed = false
            target = nil
            guard let arm, let pointer, arm.area.contains(pointer) else { return step }
            switch Self.source(processId: processId, userData: userData, injectedTag: arm.injectedTag) {
            case .trackpad: depth = 0
            case .relayed(let relayed): depth = relayed
            case .other: return step
            }
            claimed = true
            target = arm.generation
            step.verdict = .drop
            step.send = target
            step.depth = depth
            return step
        }
        guard claimed else { return step }
        // That window stopped taking gestures (released, lost the focus...): the rest of this one
        // goes nowhere, but still not to this Mac's Dock.
        if target != arm?.generation { target = nil }
        let ends = phase == Self.ended || phase == Self.cancelled
        if ends || phase == Self.changed {
            step.send = target
            step.depth = depth
        }
        if ends {
            claimed = false
            target = nil
        }
        step.verdict = phase == Self.ended && stillEnd ? .passStill : .drop
        return step
    }

    /// The tap stopped seeing events for a while (turned off, rebuilt, removed): forgets the gesture
    /// being taken. Returns the generation it was going to, to cancel it there.
    mutating func reset() -> Int? {
        defer {
            claimed = false
            target = nil
        }
        return claimed ? target : nil
    }
}

// MARK: The tap

/// Shared by the main thread and the tap's (under `tapState`'s lock).
private struct TapState: Sendable {
    /// The window taking Dock gestures, if any.
    var arm: DockGestureClaim.Arm?
    /// Whether the tap should exist.
    var wanted = false
    /// When the main thread last said it's alive.
    var lastTick: TimeInterval = 0
    var claim = DockGestureClaim()
    /// When the gesture being taken last had an event: removal stops waiting for its end after a
    /// while.
    var lastClaimedEvent: TimeInterval = 0
}

private let tapState = OSAllocatedUnfairLock(initialState: TapState())

/// The tap, on its own thread: everything here runs there, apart from `sync` and `revive`, which
/// ask it to.
private enum GestureTap {
    /// Dock gestures (CGS type 30), while the tap exists.
    nonisolated(unsafe) private static var dockTap: CFMachPort?
    nonisolated(unsafe) private static var dockSource: CFRunLoopSource?
    /// Every other trackpad gesture event (type 29), only while a Dock gesture is taken: the
    /// touches around it go too. They come with every two-finger scroll, so not all the time.
    nonisolated(unsafe) private static var touchTap: CFMachPort?
    nonisolated(unsafe) private static var touchSource: CFRunLoopSource?
    nonisolated(unsafe) private static var touchTapOn = false
    /// Removal is waiting for the end of the gesture being taken.
    nonisolated(unsafe) private static var removalPending = false

    /// A main thread silent this long can't send gestures: they stay on this Mac.
    private static let mainStallLimit: TimeInterval = 2
    /// Removal stops waiting for the end of a gesture whose events stopped this long ago.
    private static let quietLimit: TimeInterval = 5
    /// macOS 27's Dock needs the end of a gesture it never saw move (Space Rabbit).
    private static let stillEnd = ProcessInfo.processInfo.operatingSystemVersion.majorVersion >= 27

    private static let thread: TapThread = {
        let thread = TapThread()
        thread.name = "lankvm-gesture-tap"
        thread.qualityOfService = .userInteractive
        thread.startAndWait()
        return thread
    }()

    /// Creates or removes the tap to match `TapState.wanted`. Removal waits for a gesture being
    /// taken to end, unless `immediately`.
    static func sync(immediately: Bool = false) {
        perform { apply(immediately: immediately) }
    }

    /// Rebuilds the tap or turns it back on if that happened while the process was suspended
    /// (sleep, lock), when its callback hears nothing about it (Space Rabbit).
    static func revive() {
        perform { checkHealth() }
    }

    private static func perform(_ work: @escaping () -> Void) {
        let loop = thread.runLoop
        CFRunLoopPerformBlock(loop, CFRunLoopMode.defaultMode.rawValue, work)
        CFRunLoopWakeUp(loop)
    }

    private static func apply(immediately: Bool) {
        let now = ProcessInfo.processInfo.systemUptime
        let (wanted, claimed, quiet) = tapState.withLock { ($0.wanted, $0.claim.claimed, now - $0.lastClaimedEvent) }
        if wanted {
            removalPending = false
            if dockTap == nil { install() }
            return
        }
        guard dockTap != nil else { return }
        // The Dock must see all of a gesture or none of it: wait for the end (the callback asks
        // again then), unless its events stopped.
        if claimed && quiet < quietLimit && !immediately {
            removalPending = true
            let retry = CFRunLoopTimerCreateWithHandler(nil, CFAbsoluteTimeGetCurrent() + 1, 0, 0, 0) { _ in
                if removalPending { apply(immediately: false) }
            }
            CFRunLoopAddTimer(CFRunLoopGetCurrent(), retry, .defaultMode)
            return
        }
        remove()
    }

    private static func install() {
        guard let dock = makeTap(for: DockEvent.dockControlType) else { return }
        guard let touch = makeTap(for: DockEvent.gestureType) else {
            CFRunLoopRemoveSource(CFRunLoopGetCurrent(), dock.source, .defaultMode)
            CFMachPortInvalidate(dock.tap)
            return
        }
        CGEvent.tapEnable(tap: touch.tap, enable: false)
        (dockTap, dockSource, touchTap, touchSource) = (dock.tap, dock.source, touch.tap, touch.source)
        touchTapOn = false
        removalPending = false
        forget()
    }

    /// An active session tap for one event type, on this thread's run loop. Nil without
    /// Accessibility (then it isn't created, or comes back turned off and never sees an event).
    private static func makeTap(for type: UInt32) -> (tap: CFMachPort, source: CFRunLoopSource)? {
        guard let tap = CGEvent.tapCreate(tap: .cgSessionEventTap, place: .headInsertEventTap, options: .defaultTap,
                                          eventsOfInterest: CGEventMask(1) << type,
                                          callback: { _, type, event, _ in GestureTap.handle(type, event) },
                                          userInfo: nil) else { return nil }
        guard CGEvent.tapIsEnabled(tap: tap), let source = CFMachPortCreateRunLoopSource(nil, tap, 0) else {
            CFMachPortInvalidate(tap)
            return nil
        }
        CFRunLoopAddSource(CFRunLoopGetCurrent(), source, .defaultMode)
        return (tap, source)
    }

    private static func remove() {
        for (tap, source) in [(dockTap, dockSource), (touchTap, touchSource)] {
            if let source { CFRunLoopRemoveSource(CFRunLoopGetCurrent(), source, .defaultMode) }
            if let tap {
                CGEvent.tapEnable(tap: tap, enable: false)
                CFMachPortInvalidate(tap)
            }
        }
        (dockTap, dockSource, touchTap, touchSource) = (nil, nil, nil, nil)
        touchTapOn = false
        removalPending = false
        forget()
    }

    private static func checkHealth() {
        // Couldn't be created before: try again.
        guard let dockTap else { return apply(immediately: false) }
        if !CFMachPortIsValid(dockTap) || touchTap.map({ !CFMachPortIsValid($0) }) == true {
            remove()
            apply(immediately: false)
        } else if !CGEvent.tapIsEnabled(tap: dockTap) {
            forget()
            CGEvent.tapEnable(tap: dockTap, enable: true)
        }
    }

    /// Drops the gesture being taken, if any: the tap missed (or will miss) some of its events. Its
    /// window cancels it on the remote Mac.
    private static func forget() {
        if let generation = tapState.withLock({ $0.claim.reset() }) { reportLost(generation) }
        setTouchTap(false)
    }

    private static func setTouchTap(_ on: Bool) {
        guard let touchTap, on != touchTapOn else { return }
        CGEvent.tapEnable(tap: touchTap, enable: on)
        touchTapOn = on
    }

    // MARK: Events (the callback: quick, and never waits for the main thread)

    private static func handle(_ type: CGEventType, _ event: CGEvent) -> Unmanaged<CGEvent>? {
        let pass = Unmanaged.passUnretained(event)
        if type == .tapDisabledByTimeout || type == .tapDisabledByUserInput {
            // The window server turned the tap off (too slow, or for the user's own input): back on.
            // The gesture in progress is lost; the Dock got its events meanwhile.
            forget()
            if let dockTap { CGEvent.tapEnable(tap: dockTap, enable: true) }
            return pass
        }
        switch type.rawValue {
        case DockEvent.dockControlType:
            return dockControl(event)
        case DockEvent.gestureType:
            let now = ProcessInfo.processInfo.systemUptime
            let drop = tapState.withLock { state in
                if state.claim.claimed { state.lastClaimedEvent = now }
                return state.claim.claimed
            }
            return drop ? nil : pass
        default:
            return pass
        }
    }

    private static func dockControl(_ event: CGEvent) -> Unmanaged<CGEvent>? {
        let pass = Unmanaged.passUnretained(event)
        guard event.getIntegerValueField(DockEvent.subtype) == DockEvent.dockSwipe else { return pass }
        let phase = event.getIntegerValueField(DockEvent.phase)
        // Where the pointer is as a gesture begins (read the way Instant Space Switcher does).
        let pointer = phase == DockGestureClaim.began ? CGEvent(source: nil)?.location : nil
        let processId = event.getIntegerValueField(.eventSourceUnixProcessID)
        let userData = event.getIntegerValueField(.eventSourceUserData)
        let now = ProcessInfo.processInfo.systemUptime
        let (step, claimed) = tapState.withLock { state in
            // A stalled main thread couldn't send it: leave the gesture to this Mac.
            let arm = now - state.lastTick < mainStallLimit ? state.arm : nil
            let step = state.claim.dockSwipe(phase: phase, processId: processId, userData: userData, arm: arm,
                                             pointer: pointer, stillEnd: stillEnd)
            if step.verdict != .pass { state.lastClaimedEvent = now }
            return (step, state.claim.claimed)
        }
        if let lost = step.cancel { reportLost(lost) }
        if let target = step.send {
            let sample = DockSwipeSample(
                axis: UInt8(clamping: event.getIntegerValueField(DockEvent.motion)), phase: UInt8(clamping: phase),
                progress: event.getDoubleValueField(DockEvent.progress),
                velocityX: event.getDoubleValueField(DockEvent.velocityX),
                velocityY: event.getDoubleValueField(DockEvent.velocityY),
                inverted: event.getIntegerValueField(DockEvent.inverted) != 0, depth: step.depth)
            DispatchQueue.main.async {
                MainActor.assumeIsolated { DockGestures.received(sample, for: target) }
            }
        }
        setTouchTap(claimed)
        if !claimed && removalPending {
            // The gesture removal waited for has ended: remove the tap once this callback returns.
            CFRunLoopPerformBlock(CFRunLoopGetCurrent(), CFRunLoopMode.defaultMode.rawValue) { apply(immediately: false) }
        }
        switch step.verdict {
        case .pass:
            return pass
        case .drop:
            return nil
        case .passStill:
            if let still = SerializedEvent.withoutMotion(event) { return Unmanaged.passRetained(still) }
            // No IOHID record to rewrite: the fields alone, as Space Rabbit does then.
            for field in [DockEvent.progress, DockEvent.velocityX, DockEvent.velocityY] {
                event.setDoubleValueField(field, value: 0)
            }
            return pass
        }
    }

    private static func reportLost(_ generation: Int) {
        DispatchQueue.main.async {
            MainActor.assumeIsolated { DockGestures.lost(generation) }
        }
    }
}

/// A thread that only runs its run loop: the tap's events, and the work `GestureTap` sends it.
private final class TapThread: Thread {
    private(set) var runLoop: CFRunLoop!
    private let started = DispatchSemaphore(value: 0)

    override func main() {
        runLoop = CFRunLoopGetCurrent()
        // A run loop with nothing in it returns at once: a timer due every few decades keeps it
        // waiting.
        let idle = CFRunLoopTimerCreateWithHandler(nil, CFAbsoluteTimeGetCurrent() + 1e9, 1e9, 0, 0) { _ in }
        CFRunLoopAddTimer(runLoop, idle, .defaultMode)
        started.signal()
        while true { CFRunLoopRun() }
    }

    /// Starts it, and returns once it takes work.
    func startAndWait() {
        start()
        started.wait()
    }
}

/// The private event types and fields of trackpad gestures (WebKit's CoreGraphicsTestSPI.h names
/// them; Mac Mouse Fix, yabai, Instant Space Switcher and Space Rabbit read them the same way).
private enum DockEvent {
    /// CGS event types: trackpad gestures with their touches, and the Dock's gestures.
    static let gestureType: UInt32 = 29
    static let dockControlType: UInt32 = 30
    /// kCGEventGestureHIDType, the IOHIDEvent type: 23 is a Dock swipe (or pinch).
    static let subtype = field(110)
    static let dockSwipe: Int64 = 23
    /// kCGEventGesturePhase: IOHIDEventPhaseBits.
    static let phase = field(132)
    /// kCGEventGestureSwipeMotion: 1 horizontal, 2 vertical, 3 pinch.
    static let motion = field(123)
    /// kCGEventGestureSwipeProgress: travel since the gesture began (±1 is one whole transition).
    static let progress = field(124)
    /// kCGEventGestureSwipeVelocityX/Y: the exit velocity, at the end.
    static let velocityX = field(129)
    static let velocityY = field(130)
    /// The "inverted from device" flag.
    static let inverted = field(136)

    private static func field(_ number: UInt32) -> CGEventField { CGEventField(rawValue: number)! }
}

/// macOS 27's Dock reads a Dock swipe from the IOHIDEvent record attached to the event (serialized
/// field 4205), not from its fields. Rewriting the record goes through the event's public serialized
/// form (CGEventCreateData, CGEventCreateFromData), as Space Rabbit and FasterSwiper do.
private enum SerializedEvent {
    /// One field of the serialized form (version 2): a big-endian u16 size and a u16 holding a 2-bit
    /// kind and the field number, then the value.
    private struct Record {
        var field: UInt16
        var kind: UInt16
        var size: UInt16
        var bytes: Data
    }

    private static let hidEventField: UInt16 = 4205

    /// A copy of a Dock swipe with its progress and velocity zeroed, in the fields and in the
    /// record: the end of a gesture, with nothing for the Dock to act on. Nil if there's no record
    /// it can read. (The copy loses field 42, the source user data, which the trackpad's events
    /// don't use.)
    static func withoutMotion(_ event: CGEvent) -> CGEvent? {
        guard let data = event.data as Data?, var records = parse(data),
              let index = records.firstIndex(where: { $0.field == hidEventField }),
              let still = zeroingMotion(records[index].bytes) else { return nil }
        records[index].bytes = still
        guard let copy = CGEvent(withDataAllocator: kCFAllocatorDefault, data: serialize(records) as CFData) else { return nil }
        for field in [DockEvent.progress, DockEvent.velocityX, DockEvent.velocityY] {
            copy.setDoubleValueField(field, value: 0)
        }
        return copy
    }

    private static func parse(_ data: Data) -> [Record]? {
        let b = [UInt8](data)
        guard b.count >= 4, b[0] == 0, b[1] == 0, b[2] == 0, b[3] == 2 else { return nil }
        var records: [Record] = []
        var o = 4
        while o < b.count {
            guard o + 4 <= b.count else { return nil }
            let size = UInt16(b[o]) << 8 | UInt16(b[o + 1])
            let kindAndField = UInt16(b[o + 2]) << 8 | UInt16(b[o + 3])
            let kind = kindAndField >> 14
            let length: Int
            switch kind {
            case 0 where size == 1: length = 8
            case 0 where size > 1: length = Int(size)
            case 1 where size == 1: length = 4
            case 3 where size == 1: length = 4
            case 3 where size == 2: length = 8
            default: return nil
            }
            guard o + 4 + length <= b.count else { return nil }
            records.append(Record(field: kindAndField & 0x3FFF, kind: kind, size: size, bytes: Data(b[(o + 4)..<(o + 4 + length)])))
            o += 4 + length
        }
        return records
    }

    private static func serialize(_ records: [Record]) -> Data {
        var data = Data([0, 0, 0, 2])
        for r in records {
            let kindAndField = r.kind << 14 | r.field & 0x3FFF
            data.append(contentsOf: [UInt8(r.size >> 8), UInt8(r.size & 0xFF), UInt8(kindAndField >> 8), UInt8(kindAndField & 0xFF)])
            data.append(r.bytes)
        }
        return data
    }

    /// The record with its motion zeroed. It is little-endian: a 28-byte header (u64 timestamp,
    /// u64 sender, u32 options, u32 attribute length, u32 child count), the attributes, then the
    /// children, each starting with its u32 size and u32 type. A Dock swipe (23) has its 16.16
    /// progress at offset 36, a velocity (9) its x, y and z at 16 to 27. (FasterSwiper's
    /// gesture-serialization.cc, Space Rabbit.)
    private static func zeroingMotion(_ record: Data) -> Data? {
        var b = [UInt8](record)
        func u32(_ o: Int) -> Int {
            o + 4 <= b.count ? Int(UInt32(b[o]) | UInt32(b[o + 1]) << 8 | UInt32(b[o + 2]) << 16 | UInt32(b[o + 3]) << 24) : 0
        }
        guard b.count >= 28 else { return nil }
        var o = 28 + u32(20)
        for _ in 0..<u32(24) {
            guard o + 16 <= b.count else { return nil }
            let size = u32(o), type = u32(o + 4)
            guard size > 0, o + size <= b.count else { return nil }
            if type == 23 && size >= 40 { b.replaceSubrange((o + 36)..<(o + 40), with: repeatElement(0, count: 4)) }
            if type == 9 && size >= 28 { b.replaceSubrange((o + 16)..<(o + 28), with: repeatElement(0, count: 12)) }
            o += size
        }
        return Data(b)
    }
}
