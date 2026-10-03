import AppKit
import CLanKVM

/// Sends this Mac's mouse and keyboard to the remote Mac while the session controls it.
///
/// Keys are captured with an app-local event monitor, not the view's `keyDown`: the monitor sees
/// every key event before menus, SwiftUI shortcuts, the key-view loop and input methods, so ⌘Q,
/// ⌘W, Tab or Esc go to the remote Mac instead of acting here, and it also sees the key-up of
/// ⌘-shortcuts, which AppKit never delivers to views (the remote would keep that key down).
///
/// Keys travel as virtual key codes, so the remote Mac's layout and input method produce the
/// characters. Modifiers travel as state (left/right, Caps Lock, fn), re-sent whenever it changes.
/// Pressing and releasing ⌃⌥⌘ together, with no other key, stops controlling.
@MainActor
final class InputForwarder {
    let sessionId: UInt64
    private weak var view: NSView?
    private var session: SessionModel?

    /// True while input goes to the remote Mac: controlling, and this window has the focus.
    private(set) var isForwarding = false
    /// Remote video size in pixels, for mapping positions.
    var frameSize = CGSize(width: 1, height: 1)
    /// Called when the user presses the ⌃⌥⌘ chord (switches between viewing and controlling).
    var onEscape: (() -> Void)?
    /// Called whenever forwarding starts (true) or stops (false), for whatever reason.
    var onForwardingChanged: ((Bool) -> Void)?
    /// Events our own host injected that came back to this app and were dropped (same Mac).
    private(set) var droppedEchoes = 0

    private var monitor: Any?
    private var observers: [NSObjectProtocol] = []
    private var downKeys: Set<UInt16> = []
    private var downButtons: Set<Int> = []
    /// Modifier state last sent; nil when the host's state is unknown (after a release).
    private var sentModifiers: UInt64?
    /// Relay depth last sent (see `relay`); nil when unknown.
    private var sentDepth: UInt8?
    /// Modifiers already held when forwarding started (e.g. ⌘ from the ⌘Tab that brought us
    /// here). They are not sent until released, or the remote would see a phantom ⌘.
    private var preHeld: UInt64 = 0
    private var fnHeld = false
    private var chord = ChordState.idle
    private var lastPosition = CGPoint(x: 0.5, y: 0.5)
    /// Proves to the host, from this (UI) thread, that we're alive while anything is held.
    private var heartbeat: Timer?
    /// No forwarding while the display or the Mac sleeps, the screen is locked, or another user
    /// has the session; it resumes when that ends.
    private var displayAsleep = false
    private var systemAsleep = false
    private var locked = false
    private var switchedOut = false

    private enum ChordState: Equatable { case idle, armed(TimeInterval), spoiled }
    /// The chord counts only if released this soon after all three keys went down.
    private static let chordWindow: TimeInterval = 0.6

    init(sessionId: UInt64, view: NSView) {
        self.sessionId = sessionId
        self.view = view
    }

    func attach(to session: SessionModel) {
        self.session = session
        guard monitor == nil else { return }
        let mask: NSEvent.EventTypeMask = [
            .keyDown, .keyUp, .flagsChanged, .mouseMoved, .leftMouseDown, .leftMouseUp, .leftMouseDragged,
            .rightMouseDown, .rightMouseUp, .rightMouseDragged, .otherMouseDown, .otherMouseUp, .otherMouseDragged,
            .scrollWheel,
        ]
        // Local monitors run on the main thread, inside -[NSApplication sendEvent:], before the
        // event is routed to menus or windows. Returning nil drops it.
        monitor = NSEvent.addLocalMonitorForEvents(matching: mask) { [weak self] event in
            guard let self else { return event }
            return self.filter(event)
        }
        let center = NotificationCenter.default
        let workspace = NSWorkspace.shared.notificationCenter
        let refresh: @Sendable (Notification) -> Void = { [weak self] _ in
            MainActor.assumeIsolated { self?.update() }
        }
        for name in [NSApplication.didBecomeActiveNotification, NSApplication.didResignActiveNotification,
                     NSWindow.didBecomeKeyNotification, NSWindow.didResignKeyNotification,
                     NSWindow.willBeginSheetNotification, NSWindow.didEndSheetNotification] {
            observers.append(center.addObserver(forName: name, object: nil, queue: .main, using: refresh))
        }
        // Each of these pauses forwarding (releasing everything) until its counterpart.
        let pauses: [(NotificationCenter, String, String, ReferenceWritableKeyPath<InputForwarder, Bool>)] = [
            (workspace, NSWorkspace.screensDidSleepNotification.rawValue, NSWorkspace.screensDidWakeNotification.rawValue, \.displayAsleep),
            (workspace, NSWorkspace.willSleepNotification.rawValue, NSWorkspace.didWakeNotification.rawValue, \.systemAsleep),
            (workspace, NSWorkspace.sessionDidResignActiveNotification.rawValue, NSWorkspace.sessionDidBecomeActiveNotification.rawValue, \.switchedOut),
            (DistributedNotificationCenter.default(), "com.apple.screenIsLocked", "com.apple.screenIsUnlocked", \.locked),
        ]
        for (center, begin, end, flag) in pauses {
            observers.append(center.addObserver(forName: NSNotification.Name(begin), object: nil, queue: .main) { [weak self] _ in
                MainActor.assumeIsolated {
                    self?[keyPath: flag] = true
                    self?.stop()
                }
            })
            observers.append(center.addObserver(forName: NSNotification.Name(end), object: nil, queue: .main) { [weak self] _ in
                MainActor.assumeIsolated {
                    self?[keyPath: flag] = false
                    self?.update()
                }
            })
        }
        // Another Space: let go, then pick up again once the window has settled (entering full
        // screen moves it to a new Space that becomes active).
        observers.append(workspace.addObserver(forName: NSWorkspace.activeSpaceDidChangeNotification, object: nil, queue: .main) { [weak self] _ in
            MainActor.assumeIsolated {
                guard let self else { return }
                self.stop()
                DispatchQueue.main.async { [weak self] in MainActor.assumeIsolated { self?.update() } }
            }
        })
        // The Control menu's "Send System Shortcuts to Remote Mac" applies right away.
        observers.append(center.addObserver(forName: UserDefaults.didChangeNotification, object: nil, queue: .main) { [weak self] _ in
            MainActor.assumeIsolated {
                guard let self, self.isForwarding else { return }
                SystemShortcuts.capture(SystemShortcuts.enabled, owner: self)
            }
        })
    }

    func detach() {
        stop()
        if let monitor { NSEvent.removeMonitor(monitor) }
        monitor = nil
        for o in observers {
            NotificationCenter.default.removeObserver(o)
            NSWorkspace.shared.notificationCenter.removeObserver(o)
            DistributedNotificationCenter.default().removeObserver(o)
        }
        observers.removeAll()
        session = nil
    }

    /// Starts or stops forwarding to match the session and window state.
    func update() {
        guard let window = view?.window, let session else {
            stop()
            return
        }
        let should = session.isControlling && NSApp.isActive && window.isKeyWindow && window.isOnActiveSpace
            && window.attachedSheet == nil && NSApp.modalWindow == nil
            && !displayAsleep && !systemAsleep && !locked && !switchedOut
        if should && !isForwarding {
            start()
        } else if !should && isForwarding {
            stop()
        }
    }

    private func start() {
        isForwarding = true
        downKeys.removeAll()
        downButtons.removeAll()
        sentModifiers = nil
        sentDepth = nil
        chord = .idle
        // Ignore whatever is physically held right now until it's released. The HID state, not
        // the session's: on the same Mac the session state includes what the host injects.
        let held = UInt64(CGEventSource.flagsState(.hidSystemState).rawValue)
        preHeld = held & Modifiers.deviceMask
        fnHeld = false
        if SystemShortcuts.enabled {
            SystemShortcuts.capture(true, owner: self)
        }
        let timer = Timer(timeInterval: Double(HEARTBEAT_MS) / 1000, repeats: true) { [weak self] _ in
            MainActor.assumeIsolated { self?.beat() }
        }
        RunLoop.main.add(timer, forMode: .common)
        heartbeat = timer
        lk_set_focus(sessionId, true)
        onForwardingChanged?(true)
    }

    /// Stops forwarding and lets go of everything on the remote Mac.
    func stop() {
        guard isForwarding else { return }
        isForwarding = false
        SystemShortcuts.capture(false, owner: self)
        heartbeat?.invalidate()
        heartbeat = nil
        lk_input_release_all(sessionId)
        // Still controlling, just not focused: the host shows its cursor in the video meanwhile.
        if session?.isControlling == true {
            lk_set_focus(sessionId, false)
        }
        downKeys.removeAll()
        downButtons.removeAll()
        sentModifiers = nil
        sentDepth = nil
        chord = .idle
        onForwardingChanged?(false)
    }

    private func beat() {
        let holding = !downKeys.isEmpty || !downButtons.isEmpty || (sentModifiers ?? 0) & ~Modifiers.capsLock != 0
        if isForwarding && holding {
            lk_input_heartbeat(sessionId)
        }
    }

    // MARK: Events

    /// Everything the app receives passes through here first.
    private func filter(_ event: NSEvent) -> NSEvent? {
        let userData = event.cgEvent?.getIntegerValueField(.eventSourceUserData) ?? 0
        // Input our own host injected on this Mac (same-Mac session): never act on it, or it would
        // be sent back to the host in a loop. Recognised by its tag, or else by its sender.
        if let session, let cg = event.cgEvent {
            if session.injectedTag != 0 && userData & ~Self.relayDepthMask == session.injectedTag {
                droppedEchoes += 1
                return nil
            }
            if session.sameMachine && session.hostPid != 0 && cg.getIntegerValueField(.eventSourceUnixProcessID) == session.hostPid {
                droppedEchoes += 1
                return nil
            }
        }
        guard event.window === view?.window else { return event }
        // Injected by a LanKVM host: a Mac controlling this one is typing and clicking here.
        let injected = userData >> 32 == Self.injectedTagPrefix
        guard isForwarding else {
            // Viewing: the same chord switches to Control. Not a controlling Mac's chord: that
            // one is for its own window, which acts on it.
            if !injected {
                if event.type == .flagsChanged, updateChord(event) {
                    onEscape?()
                } else if event.type == .keyDown || event.type == .leftMouseDown {
                    spoilChord(event)
                }
            }
            return event
        }
        // From the very Mac this window controls: forwarding it would bounce between the two.
        // (CoreModel hands control over as soon as it notices.)
        if injected, let session, !session.hostId.isEmpty, CoreModel.shared.host.controller?.deviceId == session.hostId {
            droppedEchoes += 1
            return nil
        }
        switch event.type {
        case .keyDown:
            if !injected { spoilChord(event) }
            relay(event)
            if !event.isARepeat {
                // Re-send the modifiers with every press: the host may have let them go meanwhile
                // (it releases everything when we go quiet), and ⌘W must not arrive as W.
                sentModifiers = nil
            }
            syncModifiers(event.modifierFlags)
            if event.isARepeat {
                guard downKeys.contains(event.keyCode) else { return nil }
            } else {
                downKeys.insert(event.keyCode)
            }
            lk_input_key(sessionId, event.keyCode, true, event.isARepeat)
            return nil
        case .keyUp:
            guard downKeys.remove(event.keyCode) != nil else { return nil }
            relay(event)
            lk_input_key(sessionId, event.keyCode, false, false)
            return nil
        case .flagsChanged:
            if event.keyCode == Modifiers.fnKeyCode {
                fnHeld = event.modifierFlags.contains(.function)
            }
            if !injected && updateChord(event) {
                stop()
                onEscape?()
                return nil
            }
            relay(event)
            syncModifiers(event.modifierFlags)
            return nil
        default:
            // Mouse events reach the view; it calls the methods below.
            return event
        }
    }

    func mouseMoved(_ event: NSEvent) {
        guard isForwarding else { return }
        relay(event)
        let p = position(of: event)
        lastPosition = p
        lk_input_mouse_move(sessionId, p.x, p.y)
    }

    func mouseButton(_ event: NSEvent, down: Bool) {
        guard isForwarding else { return }
        let button: Int = switch event.type {
        case .leftMouseDown, .leftMouseUp: 0
        case .rightMouseDown, .rightMouseUp: 1
        default: event.buttonNumber
        }
        guard (0..<32).contains(button) else { return }
        if down {
            spoilChord(event)
            // A click well away from the picture (in a wide black bar) is not meant for the
            // remote Mac; near its edge it lands on the edge (menu bar, Dock).
            if distanceOutsidePicture(event) > Self.edgeSlop { return }
            downButtons.insert(button)
            sentModifiers = nil  // as for key presses
        } else if downButtons.remove(button) == nil {
            // The click that brought the window to the front: its press wasn't sent.
            return
        }
        relay(event)
        syncModifiers(event.modifierFlags)
        let p = position(of: event)
        lastPosition = p
        lk_input_mouse_button(sessionId, UInt8(button), down, UInt8(clamping: max(1, event.clickCount)), p.x, p.y)
    }

    func scroll(_ event: NSEvent) {
        guard isForwarding, let cg = event.cgEvent else { return }
        relay(event)
        syncModifiers(event.modifierFlags)
        let p = position(of: event)
        let int32 = { (field: CGEventField) in Int32(clamping: cg.getIntegerValueField(field)) }
        var scroll = lk_scroll(
            x: p.x, y: p.y,
            lines_y: int32(.scrollWheelEventDeltaAxis1), lines_x: int32(.scrollWheelEventDeltaAxis2),
            fixed_y: cg.getDoubleValueField(.scrollWheelEventFixedPtDeltaAxis1),
            fixed_x: cg.getDoubleValueField(.scrollWheelEventFixedPtDeltaAxis2),
            pixels_y: int32(.scrollWheelEventPointDeltaAxis1), pixels_x: int32(.scrollWheelEventPointDeltaAxis2),
            continuous: cg.getIntegerValueField(.scrollWheelEventIsContinuous) != 0,
            phase: UInt8(truncatingIfNeeded: cg.getIntegerValueField(.scrollWheelEventScrollPhase)),
            momentum: UInt8(truncatingIfNeeded: cg.getIntegerValueField(.scrollWheelEventMomentumPhase)),
            inverted: event.isDirectionInvertedFromDevice)
        lk_input_scroll(sessionId, &scroll)
    }

    // MARK: Helpers

    /// Tells the host, when it changes, how many LanKVM hosts the input it gets next has passed
    /// through: 0 if it was made here, one more than a host stamped on it if a Mac controlling
    /// this one injected it. Hosts drop input that went around a loop of Macs.
    private func relay(_ event: NSEvent) {
        let userData = event.cgEvent?.getIntegerValueField(.eventSourceUserData) ?? 0
        let depth: UInt8 = userData >> 32 == Self.injectedTagPrefix ? UInt8(clamping: ((userData & Self.relayDepthMask) >> 24) + 1) : 0
        if depth != sentDepth {
            sentDepth = depth
            lk_input_relayed(sessionId, depth)
        }
    }

    /// Sends the held modifiers if they changed. Flags that only describe the key itself
    /// (numeric pad, fn on arrows and F-keys) are dropped; fn counts only from the fn key.
    private func syncModifiers(_ flags: NSEvent.ModifierFlags) {
        let raw = UInt64(flags.rawValue)
        preHeld &= raw
        var state = raw & (Modifiers.deviceMask | Modifiers.familyMask | Modifiers.capsLock)
        // Drop pre-held modifiers, family flags included unless the other side's key is down.
        state &= ~preHeld
        for (family, devices) in Modifiers.families where state & devices == 0 && raw & family != 0 && preHeld & devices != 0 {
            state &= ~family
        }
        if fnHeld { state |= Modifiers.fn }
        if state != sentModifiers {
            sentModifiers = state
            lk_input_modifiers(sessionId, state)
        }
    }

    /// A key or click while chord modifiers are held means it was a shortcut, not the chord.
    /// (With nothing held there is no chord in progress to spoil.)
    private func spoilChord(_ event: NSEvent) {
        if !event.modifierFlags.intersection([.control, .option, .command, .shift]).isEmpty {
            chord = .spoiled
        }
    }

    /// Tracks the ⌃⌥⌘ chord: all three down, then all up within a moment, with no other key,
    /// click or ⇧ in between (so ⌃⌥⌘-letter shortcuts and Hyper keys don't trigger it).
    /// Returns true when it completes.
    private func updateChord(_ event: NSEvent) -> Bool {
        let held = event.modifierFlags.intersection([.control, .option, .command, .shift])
        switch chord {
        case .idle where held == [.control, .option, .command]:
            chord = .armed(event.timestamp)
        case .armed where held.contains(.shift):
            chord = .spoiled
        case .armed(let at) where held.isEmpty:
            chord = .idle
            return event.timestamp - at <= Self.chordWindow
        case .spoiled where held.isEmpty:
            chord = .idle
        default:
            break
        }
        return false
    }

    /// Where on the remote screen an event happened, 0...1 from its left/top edge. Uses the same
    /// letterbox math as the renderer (crates/core/src/render.rs), in drawable pixels.
    private func position(of event: NSEvent) -> CGPoint {
        guard let view, let window = view.window, view.bounds.width > 0, view.bounds.height > 0 else { return lastPosition }
        let p = view.convert(event.locationInWindow, from: nil)
        let scale = window.backingScaleFactor
        let tw = max(1, (view.bounds.width * scale).rounded())
        let th = max(1, (view.bounds.height * scale).rounded())
        let rect = Self.letterbox(frame: frameSize, target: CGSize(width: tw, height: th))
        let px = p.x * tw / view.bounds.width
        let py = (view.bounds.height - p.y) * th / view.bounds.height
        return CGPoint(x: min(max((px - rect.minX) / rect.width, 0), 1),
                       y: min(max((py - rect.minY) / rect.height, 0), 1))
    }

    /// How far (points) outside the picture a click may be and still count, clamped to its edge.
    private static let edgeSlop: CGFloat = 24

    /// The remote picture in view coordinates (points, bottom-left origin).
    func pictureRect() -> CGRect? {
        guard let view, let window = view.window, view.bounds.width > 0, view.bounds.height > 0 else { return nil }
        let scale = window.backingScaleFactor
        let tw = max(1, (view.bounds.width * scale).rounded())
        let th = max(1, (view.bounds.height * scale).rounded())
        let r = Self.letterbox(frame: frameSize, target: CGSize(width: tw, height: th))
        let sx = view.bounds.width / tw, sy = view.bounds.height / th
        return CGRect(x: r.minX * sx, y: view.bounds.height - r.maxY * sy, width: r.width * sx, height: r.height * sy)
    }

    private func distanceOutsidePicture(_ event: NSEvent) -> CGFloat {
        guard let view, let rect = pictureRect() else { return 0 }
        let p = view.convert(event.locationInWindow, from: nil)
        let dx = max(rect.minX - p.x, 0, p.x - rect.maxX)
        let dy = max(rect.minY - p.y, 0, p.y - rect.maxY)
        return max(dx, dy)
    }

    nonisolated static func letterbox(frame: CGSize, target: CGSize) -> CGRect {
        let fw = max(frame.width, 1), fh = max(frame.height, 1)
        let scale = min(target.width / fw, target.height / fh)
        let w = (fw * scale).rounded(), h = (fh * scale).rounded()
        return CGRect(x: ((target.width - w) / 2).rounded(.down), y: ((target.height - h) / 2).rounded(.down), width: max(w, 1), height: max(h, 1))
    }
}

/// How often the UI thread tells the host it's alive while holding input (protocol constant).
private let HEARTBEAT_MS = 250

extension InputForwarder {
    /// High half of the source user data of every event a LanKVM host injects ("LKVM"); below
    /// it, 8 bits of relay depth, then the host's own 24 bits (crates/platform-mac inject.rs).
    static let injectedTagPrefix: Int64 = 0x4C4B564D
    static let relayDepthMask: Int64 = 0xFF << 24
}

/// Modifier bits of `CGEventFlags` / `NSEvent.ModifierFlags` (IOLLEvent.h), as the core expects.
enum Modifiers {
    static let deviceMask: UInt64 = 0x1 | 0x2 | 0x4 | 0x8 | 0x10 | 0x20 | 0x40 | 0x2000
    static let capsLock: UInt64 = 0x10000
    static let familyMask: UInt64 = 0x20000 | 0x40000 | 0x80000 | 0x100000
    static let fn: UInt64 = 0x800000
    static let fnKeyCode: UInt16 = 63
    /// (family flag, its left and right device bits): shift, control, option, command.
    static let families: [(UInt64, UInt64)] = [(0x20000, 0x2 | 0x4), (0x40000, 0x1 | 0x2000), (0x80000, 0x20 | 0x40), (0x100000, 0x8 | 0x10)]
}
