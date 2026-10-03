// LanKVM Input Lab: a target app for testing remote control. It logs every mouse, scroll,
// keyboard and trackpad gesture event it receives as JSON lines, so a test can check exactly
// what a LanKVM host injected, and flips a large patch between black and white on every key or
// click, so a viewer can measure input-to-photon latency from the video.
//
//   swiftc -O scripts/input-lab.swift -o target/input-lab
//   target/input-lab --log /tmp/input-lab.jsonl [--frame X,Y,W,H]
//
// --frame places the window, in screen points with a top-left origin. A "window" log line
// describes the window, the patch, the text box and the scroll area in the same coordinates,
// and is repeated whenever the window moves or resizes.

import AppKit

struct Options {
    var log = "/tmp/input-lab.jsonl"
    var frame = CGRect(x: 60, y: 80, width: 960, height: 640)

    init() {
        var args = CommandLine.arguments.dropFirst().makeIterator()
        while let arg = args.next() {
            switch arg {
            case "--log": log = args.next() ?? log
            case "--frame":
                let parts = (args.next() ?? "").split(separator: ",").compactMap { Double($0) }
                if parts.count == 4 { frame = CGRect(x: parts[0], y: parts[1], width: parts[2], height: parts[3]) }
            default: break
            }
        }
    }
}

let options = Options()

/// Appends one JSON object per line and flushes it immediately.
final class EventLog {
    private let handle: FileHandle
    private let start = ProcessInfo.processInfo.systemUptime
    private var seq = 0

    init(path: String) {
        FileManager.default.createFile(atPath: path, contents: nil)
        handle = FileHandle(forWritingAtPath: path)!
    }

    func write(_ type: String, _ fields: [String: Any]) {
        seq += 1
        var object = fields
        object["type"] = type
        object["seq"] = seq
        object["t"] = ((ProcessInfo.processInfo.systemUptime - start) * 1000).rounded() / 1000
        // Mach-based uptime in µs: the same clock the LanKVM core uses, for latency math.
        object["uptime_us"] = Int(ProcessInfo.processInfo.systemUptime * 1_000_000)
        guard let data = try? JSONSerialization.data(withJSONObject: object, options: [.sortedKeys]) else { return }
        handle.write(data)
        handle.write(Data("\n".utf8))
    }
}

let eventLog = EventLog(path: options.log)

/// Converts a bottom-left-origin screen point to the top-left-origin global space CGEvent uses.
func topLeft(_ p: NSPoint) -> [Double] {
    let height = NSScreen.screens.first?.frame.height ?? 0
    return [Double(p.x), Double(height - p.y)]
}

func topLeftRect(_ r: NSRect) -> [Double] {
    let height = NSScreen.screens.first?.frame.height ?? 0
    return [Double(r.minX), Double(height - r.maxY), Double(r.width), Double(r.height)]
}

func buttonName(_ event: NSEvent) -> String {
    switch event.type {
    case .leftMouseDown, .leftMouseUp, .leftMouseDragged: "left"
    case .rightMouseDown, .rightMouseUp, .rightMouseDragged: "right"
    default: "other\(event.buttonNumber)"
    }
}

func phaseName(_ phase: NSEvent.Phase) -> String {
    var names: [String] = []
    if phase.contains(.began) { names.append("began") }
    if phase.contains(.stationary) { names.append("stationary") }
    if phase.contains(.changed) { names.append("changed") }
    if phase.contains(.ended) { names.append("ended") }
    if phase.contains(.cancelled) { names.append("cancelled") }
    if phase.contains(.mayBegin) { names.append("mayBegin") }
    return names.joined(separator: "|")
}

/// Fields every event carries: where it came from (pid, the LanKVM tag in source user data)
/// and the device-dependent modifier bits.
func common(_ event: NSEvent) -> [String: Any] {
    var fields: [String: Any] = [
        "flags": event.modifierFlags.rawValue,
        "loc": topLeft(NSEvent.mouseLocation),
    ]
    if let cg = event.cgEvent {
        fields["cg_loc"] = [Double(cg.location.x), Double(cg.location.y)]
        fields["src_pid"] = cg.getIntegerValueField(.eventSourceUnixProcessID)
        fields["src_user_data"] = cg.getIntegerValueField(.eventSourceUserData)
        fields["cg_flags"] = cg.flags.rawValue
    }
    return fields
}

/// The big square that flips colour on input.
final class PatchView: NSView {
    var white = false { didSet { needsDisplay = true } }
    override func draw(_ dirtyRect: NSRect) {
        (white ? NSColor.white : NSColor.black).setFill()
        bounds.fill()
    }
}

final class LabController: NSObject, NSWindowDelegate, NSTextViewDelegate {
    let window: NSWindow
    let patch = PatchView()
    let textView: NSTextView
    let scrollView = NSScrollView()
    let status = NSTextField(labelWithString: "")
    var counts: [String: Int] = [:]

    override init() {
        let screenHeight = NSScreen.screens.first?.frame.height ?? 1000
        let f = options.frame
        let contentRect = NSRect(x: f.minX, y: screenHeight - f.minY - f.height, width: f.width, height: f.height)
        window = NSWindow(contentRect: contentRect, styleMask: [.titled, .closable, .miniaturizable, .resizable], backing: .buffered, defer: false)
        window.title = "LanKVM Input Lab"
        window.isReleasedWhenClosed = false

        let textScroll = NSTextView.scrollableTextView()
        textView = textScroll.documentView as! NSTextView
        super.init()

        let content = NSView(frame: NSRect(origin: .zero, size: f.size))
        content.autoresizesSubviews = false
        window.contentView = content

        // Layout in window points (bottom-left origin): patch top-left, text top-right,
        // scrollable list bottom-right, status line along the bottom.
        let pad: CGFloat = 16
        let patchSize: CGFloat = 280
        patch.frame = NSRect(x: pad, y: f.height - pad - patchSize, width: patchSize, height: patchSize)
        content.addSubview(patch)

        textScroll.frame = NSRect(x: patchSize + 2 * pad, y: f.height / 2 + pad / 2, width: f.width - patchSize - 3 * pad, height: f.height / 2 - 1.5 * pad)
        textView.font = .monospacedSystemFont(ofSize: 15, weight: .regular)
        textView.delegate = self
        textView.isAutomaticQuoteSubstitutionEnabled = false
        textView.isAutomaticDashSubstitutionEnabled = false
        textView.isAutomaticTextReplacementEnabled = false
        textView.isAutomaticSpellingCorrectionEnabled = false
        content.addSubview(textScroll)

        scrollView.frame = NSRect(x: patchSize + 2 * pad, y: 40, width: f.width - patchSize - 3 * pad, height: f.height / 2 - 40 - pad / 2)
        scrollView.hasVerticalScroller = true
        scrollView.hasHorizontalScroller = true
        let list = NSTextView(frame: NSRect(x: 0, y: 0, width: 1400, height: 6000))
        list.isEditable = false
        list.isSelectable = false
        list.string = (1...200).map { "Row \($0) — scroll target. ".appending(String(repeating: "wide ", count: 40)) }.joined(separator: "\n")
        list.isHorizontallyResizable = true
        list.textContainer?.widthTracksTextView = false
        list.textContainer?.containerSize = NSSize(width: 4000, height: CGFloat.greatestFiniteMagnitude)
        scrollView.documentView = list
        scrollView.contentView.postsBoundsChangedNotifications = true
        NotificationCenter.default.addObserver(self, selector: #selector(scrolled), name: NSView.boundsDidChangeNotification, object: scrollView.contentView)
        content.addSubview(scrollView)

        status.frame = NSRect(x: pad, y: 10, width: f.width - 2 * pad, height: 20)
        status.font = .monospacedSystemFont(ofSize: 11, weight: .regular)
        status.textColor = .secondaryLabelColor
        content.addSubview(status)

        window.delegate = self
    }

    func show() {
        window.makeKeyAndOrderFront(nil)
        window.makeFirstResponder(textView)
        NSApp.activate(ignoringOtherApps: true)
        describeWindow()
    }

    /// Where everything is, in top-left global points, so a test can aim at it.
    func describeWindow() {
        let patchRect = window.convertToScreen(patch.convert(patch.bounds, to: nil))
        let textRect = window.convertToScreen(textView.enclosingScrollView!.convert(textView.enclosingScrollView!.bounds, to: nil))
        let scrollRect = window.convertToScreen(scrollView.convert(scrollView.bounds, to: nil))
        let screen = NSScreen.screens.first?.frame ?? .zero
        eventLog.write("window", [
            "frame": topLeftRect(window.frame),
            "content": topLeftRect(window.convertToScreen(window.contentView!.frame)),
            "patch": topLeftRect(patchRect),
            "text": topLeftRect(textRect),
            "scroll": topLeftRect(scrollRect),
            "screen": [Double(screen.width), Double(screen.height)],
            "pid": Int(ProcessInfo.processInfo.processIdentifier),
        ])
    }

    func windowDidMove(_ notification: Notification) { describeWindow() }
    func windowDidResize(_ notification: Notification) { describeWindow() }
    func windowDidBecomeKey(_ notification: Notification) { eventLog.write("focus", ["key": true]) }
    func windowDidResignKey(_ notification: Notification) { eventLog.write("focus", ["key": false]) }

    func textDidChange(_ notification: Notification) {
        eventLog.write("text", ["value": textView.string])
    }

    @objc func scrolled() {
        let origin = scrollView.contentView.bounds.origin
        eventLog.write("scrolled", ["x": Double(origin.x), "y": Double(origin.y)])
    }

    func record(_ event: NSEvent) {
        var fields = common(event)
        let name: String
        switch event.type {
        case .mouseMoved:
            name = "move"
        case .leftMouseDragged, .rightMouseDragged, .otherMouseDragged:
            name = "drag"
            fields["button"] = buttonName(event)
        case .leftMouseDown, .rightMouseDown, .otherMouseDown:
            name = "down"
            fields["button"] = buttonName(event)
            fields["clicks"] = event.clickCount
            patch.white.toggle()
        case .leftMouseUp, .rightMouseUp, .otherMouseUp:
            name = "up"
            fields["button"] = buttonName(event)
            fields["clicks"] = event.clickCount
        case .scrollWheel:
            name = "scroll"
            fields["dx"] = Double(event.scrollingDeltaX)
            fields["dy"] = Double(event.scrollingDeltaY)
            fields["precise"] = event.hasPreciseScrollingDeltas
            fields["phase"] = phaseName(event.phase)
            fields["momentum"] = phaseName(event.momentumPhase)
            fields["inverted"] = event.isDirectionInvertedFromDevice
        case .keyDown, .keyUp:
            name = event.type == .keyDown ? "keydown" : "keyup"
            fields["code"] = Int(event.keyCode)
            fields["chars"] = event.characters ?? ""
            fields["chars_raw"] = event.charactersIgnoringModifiers ?? ""
            fields["repeat"] = event.isARepeat
            if event.type == .keyDown && !event.isARepeat { patch.white.toggle() }
        case .flagsChanged:
            name = "flags"
            fields["code"] = Int(event.keyCode)
        case .magnify:
            name = "magnify"
            fields["amount"] = Double(event.magnification)
            fields["phase"] = phaseName(event.phase)
        case .rotate:
            name = "rotate"
            fields["degrees"] = Double(event.rotation)
            fields["phase"] = phaseName(event.phase)
        case .smartMagnify:
            name = "smart_magnify"
        case .swipe:
            name = "swipe"
            fields["dx"] = Double(event.deltaX)
            fields["dy"] = Double(event.deltaY)
            fields["phase"] = phaseName(event.phase)
        case .beginGesture, .endGesture:
            name = event.type == .beginGesture ? "begin_gesture" : "end_gesture"
        default:
            return
        }
        counts[name, default: 0] += 1
        eventLog.write(name, fields)
        status.stringValue = counts.sorted { $0.key < $1.key }.map { "\($0.key) \($0.value)" }.joined(separator: "  ")
            + "   patch=\(patch.white ? "white" : "black")   log: \(options.log)"
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate {
    var lab: LabController!

    func applicationDidFinishLaunching(_ notification: Notification) {
        buildMenu()
        lab = LabController()
        let mask: NSEvent.EventTypeMask = [
            .mouseMoved, .leftMouseDown, .leftMouseUp, .leftMouseDragged, .rightMouseDown, .rightMouseUp,
            .rightMouseDragged, .otherMouseDown, .otherMouseUp, .otherMouseDragged, .scrollWheel,
            .keyDown, .keyUp, .flagsChanged, .magnify, .rotate, .smartMagnify, .swipe, .beginGesture, .endGesture,
        ]
        NSEvent.addLocalMonitorForEvents(matching: mask) { [weak self] event in
            self?.lab.record(event)
            return event
        }
        lab.window.acceptsMouseMovedEvents = true
        lab.show()
        eventLog.write("ready", ["log": options.log])
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { true }

    func applicationWillTerminate(_ notification: Notification) {
        eventLog.write("quit", [:])
    }

    /// App and Edit menus, so ⌘Q, ⌘A, ⌘C, ⌘V and ⌘Z behave as in any Mac app.
    private func buildMenu() {
        let main = NSMenu()
        let appItem = NSMenuItem()
        let appMenu = NSMenu()
        appMenu.addItem(withTitle: "Quit Input Lab", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")
        appItem.submenu = appMenu
        main.addItem(appItem)

        let editItem = NSMenuItem()
        let edit = NSMenu(title: "Edit")
        edit.addItem(withTitle: "Undo", action: Selector(("undo:")), keyEquivalent: "z")
        edit.addItem(withTitle: "Redo", action: Selector(("redo:")), keyEquivalent: "Z")
        edit.addItem(.separator())
        edit.addItem(withTitle: "Cut", action: #selector(NSText.cut(_:)), keyEquivalent: "x")
        edit.addItem(withTitle: "Copy", action: #selector(NSText.copy(_:)), keyEquivalent: "c")
        edit.addItem(withTitle: "Paste", action: #selector(NSText.paste(_:)), keyEquivalent: "v")
        edit.addItem(withTitle: "Select All", action: #selector(NSText.selectAll(_:)), keyEquivalent: "a")
        editItem.submenu = edit
        main.addItem(editItem)
        NSApp.mainMenu = main
    }
}

let app = NSApplication.shared
app.setActivationPolicy(.regular)
let delegate = AppDelegate()
app.delegate = delegate
app.run()
