// LanKVM Frame Source: deterministic screen content for latency and frame-rate tests. A window
// (or a whole screen) is redrawn with Metal on every display refresh, up to --hz, and every frame
// carries its number n in a strip of 16 black/white squares (Gray code), so a viewer can tell
// exactly which source frame it shows and when that frame was on the host's screen.
//
//   swiftc -O scripts/frame-source.swift -o target/frame-source
//   target/frame-source [--screen main|virtual|N|display:ID] [--fullscreen | --frame X,Y,W,H] [--hz 120]
//                       [--change tiny|small|band|scroll|full] [--log FILE] [--seconds S] [--click]
//                       [--drawables 2|3] [--no-sync] [--background]
//
// --screen picks the screen (virtual: a LanKVM virtual display; N: NSScreen.screens[N];
// display:ID: the display with that CGDirectDisplayID); --frame places the window in
// that screen's points, top-left origin; --fullscreen covers the whole screen with a borderless
// window. --change is what changes besides the frame number:
//   tiny    nothing else (the strip is 512×32 points, top left)
//   small   a 128×128 point patch flips black/white every frame
//   band    a quarter of the height scrolls text-like blocks, 4 px a frame
//   scroll  the whole window scrolls text-like blocks, 4 px a frame
//   full    the whole window shows new text-like blocks every frame
// --click: the patch flips only on mouse down (for input-to-photon tests), logged with the
// event's timestamp. --background: no Dock icon, menu bar or focus taken from the app in front
// (for unattended runs on a screen nobody looks at, e.g. scripts/latency-bench.sh).
//
// The strip shows n modulo 65536 (16 squares), so it wraps to 0 after 65535.
//
// The log (JSON lines, all times in µs of the mach clock LanKVM uses) has one "frame" line per
// frame: {n, target_us, commit_us, presented_us} (presented_us 0 if it never reached the
// screen), a "window" line describing where the strip is (strip_points: x, y, w, h in the
// screen's points from its top-left corner; screen_points; scale; squares), and "click" lines.

import AppKit
import Metal
import QuartzCore

struct Options {
    var screen = "main"
    var fullscreen = false
    var frame = CGRect(x: 80, y: 80, width: 1200, height: 800)
    var hz = 120
    var change = "small"
    var log: String? = nil
    var seconds: Double? = nil
    var click = false
    var drawables = 2
    /// Draw from a thread at random times averaging this rate, like frames arriving off a
    /// network, instead of in step with the display.
    var timerHz: Double? = nil
    /// Something over the Metal layer: none, tab (a small opaque view), glass (a small
    /// translucent material view), hud (a larger translucent panel).
    var overlay = "none"
    /// With --timer: also run an idle display link asking for this rate, to see whether it
    /// keeps the compositor at full rate.
    var paceHz: Float? = nil
    /// With --timer: re-present the same picture every this many ms for a second after each
    /// new frame (logged as "warm" frames, not counted as frames).
    var warmMs: Double? = nil
    var displaySync = true
    var background = false

    init() {
        var args = CommandLine.arguments.dropFirst().makeIterator()
        while let arg = args.next() {
            switch arg {
            case "--screen": screen = args.next() ?? screen
            case "--fullscreen": fullscreen = true
            case "--frame":
                let p = (args.next() ?? "").split(separator: ",").compactMap { Double($0) }
                if p.count == 4 { frame = CGRect(x: p[0], y: p[1], width: p[2], height: p[3]) }
            case "--hz": hz = Int(args.next() ?? "") ?? hz
            case "--change": change = args.next() ?? change
            case "--log": log = args.next()
            case "--seconds": seconds = Double(args.next() ?? "")
            case "--click": click = true
            case "--drawables": drawables = Int(args.next() ?? "") ?? drawables
            case "--no-sync": displaySync = false
            case "--timer": timerHz = Double(args.next() ?? "")
            case "--overlay": overlay = args.next() ?? overlay
            case "--pace": paceHz = Float(args.next() ?? "")
            case "--warm": warmMs = Double(args.next() ?? "")
            case "--background": background = true
            case "-h", "--help":
                print("usage: frame-source [--screen main|virtual|N|display:ID] [--fullscreen | --frame X,Y,W,H] [--hz N] [--change tiny|small|band|scroll|full] [--log FILE] [--seconds S] [--click] [--background]")
                exit(0)
            default:
                FileHandle.standardError.write(Data("unknown argument \(arg)\n".utf8))
                exit(2)
            }
        }
        guard ["tiny", "small", "band", "scroll", "full"].contains(change) else {
            FileHandle.standardError.write(Data("--change must be tiny, small, band, scroll or full\n".utf8))
            exit(2)
        }
    }
}

let options = Options()

func nowUs() -> Int { Int(CACurrentMediaTime() * 1_000_000) }

/// JSON lines, written on a queue of their own so presented handlers never block on I/O.
final class Log {
    private let handle: FileHandle?
    private let queue = DispatchQueue(label: "frame-source.log")

    init(path: String?) {
        guard let path else { handle = nil; return }
        FileManager.default.createFile(atPath: path, contents: nil)
        handle = FileHandle(forWritingAtPath: path)
    }

    func write(_ type: String, _ fields: [String: Any]) {
        guard let handle else { return }
        var object = fields
        object["type"] = type
        queue.async {
            guard let data = try? JSONSerialization.data(withJSONObject: object, options: [.sortedKeys]) else { return }
            handle.write(data)
            handle.write(Data("\n".utf8))
        }
    }

    func flush() { queue.sync {} }
}

let log = Log(path: options.log)

let shaderSource = """
#include <metal_stdlib>
using namespace metal;
struct Rect { float4 r; float4 color; };   // x, y, w, h in pixels, top-left origin
struct Out { float4 pos [[position]]; float4 color; };
vertex Out vs(uint v [[vertex_id]], uint i [[instance_id]], constant Rect *rects [[buffer(0)]], constant float2 &size [[buffer(1)]]) {
    float2 corner = float2(float(v & 1u), float((v >> 1u) & 1u));
    float2 p = rects[i].r.xy + corner * rects[i].r.zw;
    Out o;
    o.pos = float4(p.x / size.x * 2.0 - 1.0, 1.0 - p.y / size.y * 2.0, 0.0, 1.0);
    o.color = rects[i].color;
    return o;
}
fragment float4 fs(Out in [[stage_in]]) { return in.color; }
"""

struct GPURect {
    var r: SIMD4<Float>
    var color: SIMD4<Float>
}

/// A cheap position hash, so text-like blocks are the same wherever they scroll to.
func blockHash(_ a: Int, _ b: Int) -> UInt32 {
    var h = UInt32(truncatingIfNeeded: a &* 73_856_093) ^ UInt32(truncatingIfNeeded: b &* 19_349_663)
    h ^= h >> 13
    h = h &* 0x5bd1_e995
    h ^= h >> 15
    return h
}

final class SourceView: NSView {
    let device = MTLCreateSystemDefaultDevice()!
    lazy var queue = device.makeCommandQueue()!
    lazy var pipeline: MTLRenderPipelineState = {
        let library = try! device.makeLibrary(source: shaderSource, options: nil)
        let desc = MTLRenderPipelineDescriptor()
        desc.vertexFunction = library.makeFunction(name: "vs")
        desc.fragmentFunction = library.makeFunction(name: "fs")
        desc.colorAttachments[0].pixelFormat = .bgra8Unorm
        return try! device.makeRenderPipelineState(descriptor: desc)
    }()
    let metalLayer = CAMetalLayer()
    var n = 0
    var clickWhite = false
    /// A click waits for the next frame to show it.
    var pendingClick: [String: Any]? = nil
    var started = CACurrentMediaTime()
    var link: CADisplayLink?
    var ticks = 0
    var skipped = 0

    override init(frame: NSRect) {
        super.init(frame: frame)
        wantsLayer = true
        metalLayer.device = device
        metalLayer.pixelFormat = .bgra8Unorm
        metalLayer.framebufferOnly = true
        metalLayer.isOpaque = true
        metalLayer.maximumDrawableCount = options.drawables
        metalLayer.displaySyncEnabled = options.displaySync
    }

    required init?(coder: NSCoder) { fatalError() }

    override func makeBackingLayer() -> CALayer { metalLayer }
    override var acceptsFirstResponder: Bool { true }
    /// Read on the drawing thread, kept up to date from the main thread.
    var visible = true
    var occlusionObserver: Any?

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        guard let window else { return }
        metalLayer.contentsScale = window.backingScaleFactor
        updateSize()
        occlusionObserver = NotificationCenter.default.addObserver(forName: NSWindow.didChangeOcclusionStateNotification, object: window, queue: .main) { [weak self] _ in
            self?.visible = window.occlusionState.contains(.visible)
        }
        if let hz = options.timerHz {
            Thread.detachNewThread { [weak self] in
                while let self {
                    // Uniform between half and one and a half periods.
                    let wait = (0.5 + Double.random(in: 0..<1)) / hz
                    if let warm = options.warmMs {
                        // Re-present every `warm` ms meanwhile, for at most a second.
                        let due = CACurrentMediaTime() + wait
                        let start = CACurrentMediaTime()
                        while true {
                            let left = due - CACurrentMediaTime()
                            if left <= 0 { break }
                            if CACurrentMediaTime() - start > 1 {
                                usleep(useconds_t(left * 1_000_000))
                                break
                            }
                            usleep(useconds_t(min(left, warm / 1000) * 1_000_000))
                            if due - CACurrentMediaTime() > 0.001 { autoreleasepool { self.draw(target: 0, warm: true) } }
                        }
                    } else {
                        usleep(useconds_t(wait * 1_000_000))
                    }
                    if let seconds = options.seconds, CACurrentMediaTime() - self.started > seconds {
                        log.write("summary", ["drawn": self.n, "skipped": self.skipped])
                        log.flush()
                        exit(0)
                    }
                    autoreleasepool { self.draw(target: 0) }
                }
            }
            if let pace = options.paceHz {
                let idle = displayLink(target: self, selector: #selector(idleTick(_:)))
                idle.preferredFrameRateRange = CAFrameRateRange(minimum: pace, maximum: pace, preferred: pace)
                idle.add(to: .main, forMode: .common)
                self.link = idle
            }
            describe()
            return
        }
        let link = displayLink(target: self, selector: #selector(tick(_:)))
        let hz = Float(options.hz)
        link.preferredFrameRateRange = CAFrameRateRange(minimum: hz, maximum: hz, preferred: hz)
        link.add(to: .main, forMode: .common)
        self.link = link
        describe()
    }

    override func setFrameSize(_ newSize: NSSize) {
        super.setFrameSize(newSize)
        updateSize()
    }

    func updateSize() {
        let scale = window?.backingScaleFactor ?? 2
        metalLayer.drawableSize = CGSize(width: bounds.width * scale, height: bounds.height * scale)
    }

    /// The strip's place on screen, in global pixels (top-left origin) and points.
    func describe() {
        guard let window, let screen = window.screen else { return }
        let scale = window.backingScaleFactor
        let inWindow = convert(NSRect(x: 16, y: bounds.height - 16 - 32, width: 512, height: 32), to: nil)
        let onScreen = window.convertToScreen(inWindow)
        // Points from the screen's own top-left corner.
        let local = CGRect(x: onScreen.minX - screen.frame.minX, y: screen.frame.maxY - onScreen.maxY, width: onScreen.width, height: onScreen.height)
        let id = (screen.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? NSNumber)?.uint32Value ?? 0
        log.write("window", [
            "display_id": Int(id),
            "scale": Double(scale),
            "screen_points": [Double(screen.frame.width), Double(screen.frame.height)],
            "strip_points": [Double(local.minX), Double(local.minY), Double(local.width), Double(local.height)],
            "squares": 16,
            "hz": options.hz,
            "change": options.change,
            "max_fps": screen.maximumFramesPerSecond,
        ])
    }

    override func mouseDown(with event: NSEvent) {
        guard options.click else { return }
        clickWhite.toggle()
        pendingClick = ["event_us": Int(event.timestamp * 1_000_000), "handled_us": nowUs(), "white": clickWhite]
    }

    @objc func idleTick(_ link: CADisplayLink) {}

    @objc func tick(_ link: CADisplayLink) {
        ticks += 1
        if let seconds = options.seconds, CACurrentMediaTime() - started > seconds {
            log.write("summary", ["ticks": ticks, "drawn": n, "skipped": skipped, "occlusion": Int(window?.occlusionState.rawValue ?? 0), "drawable_size": [Double(metalLayer.drawableSize.width), Double(metalLayer.drawableSize.height)]])
            log.flush()
            exit(0)
        }
        draw(target: link.targetTimestamp)
    }

    func rects(size: CGSize, scale: CGFloat) -> [GPURect] {
        var out: [GPURect] = []
        let s = Float(scale)
        let white = SIMD4<Float>(1, 1, 1, 1), black = SIMD4<Float>(0, 0, 0, 1)
        let (w, h) = (Float(size.width), Float(size.height))
        // Background.
        out.append(GPURect(r: SIMD4(0, 0, w, h), color: SIMD4(0.12, 0.12, 0.14, 1)))
        // Text-like blocks in [y0, y1) scrolled by `offset` pixels, from text seed `seed`.
        func text(y0: Float, y1: Float, offset: Int, seed: Int) {
            out.append(GPURect(r: SIMD4(0, y0, w, y1 - y0), color: SIMD4(0.97, 0.97, 0.96, 1)))
            let line = Int(22 * s), glyph = Int(9 * s)
            let first = (Int(y0) + offset) / line
            let last = (Int(y1) + offset) / line + 1
            for l in first...last {
                let top = Float(l * line - offset)
                var x = Int(24 * s)
                let ink = SIMD4<Float>(0.1, 0.1, 0.12, 1)
                var word = 0
                while x < Int(w) - glyph {
                    let len = Int(blockHash(l &+ seed, word) % 9) + 2
                    let wpx = len * glyph
                    let gy0 = max(top + Float(5 * s), y0), gy1 = min(top + Float(5 * s) + Float(12 * s), y1)
                    if gy1 > gy0 && blockHash(l, word &+ seed) % 11 != 0 {
                        out.append(GPURect(r: SIMD4(Float(x), gy0, Float(wpx), gy1 - gy0), color: ink))
                    }
                    x += wpx + glyph
                    word += 1
                }
            }
        }
        switch options.change {
        case "band": text(y0: h * 0.375, y1: h * 0.625, offset: n * 4, seed: 0)
        case "scroll": text(y0: 0, y1: h, offset: n * 4, seed: 0)
        case "full": text(y0: 0, y1: h, offset: 0, seed: n &* 7919)
        default: break
        }
        // Patch.
        let patchWhite = options.click ? clickWhite : (options.change == "small" && n % 2 == 1)
        if options.change == "small" || options.click {
            out.append(GPURect(r: SIMD4(600 * s, 200 * s, 128 * s, 128 * s), color: patchWhite ? white : black))
        }
        // Frame number strip, last so nothing covers it: Gray code, bit 0 leftmost.
        // Of n modulo 2^16: the Gray code of a bigger number doesn't wrap to 0 in 16 bits.
        let low = UInt32(truncatingIfNeeded: n) & 0xFFFF
        let gray = low ^ (low >> 1)
        let side = 32 * s
        out.append(GPURect(r: SIMD4(16 * s - 4 * s, 16 * s - 4 * s, side * 16 + 8 * s, side + 8 * s), color: SIMD4(0.5, 0.5, 0.5, 1)))
        for bit in 0..<16 {
            let on = (gray >> UInt32(bit)) & 1 == 1
            out.append(GPURect(r: SIMD4(16 * s + Float(bit) * side, 16 * s, side, side), color: on ? white : black))
        }
        return out
    }

    func draw(target: CFTimeInterval, warm: Bool = false) {
        guard visible, let drawable = metalLayer.nextDrawable() else {
            skipped += 1
            return
        }
        let scale = window?.backingScaleFactor ?? 2
        let size = CGSize(width: bounds.width * scale, height: bounds.height * scale)
        if warm { n -= 1 }
        var list = rects(size: size, scale: scale)
        if warm { n += 1 }
        var viewport = SIMD2<Float>(Float(size.width), Float(size.height))
        let pass = MTLRenderPassDescriptor()
        pass.colorAttachments[0].texture = drawable.texture
        pass.colorAttachments[0].loadAction = .clear
        pass.colorAttachments[0].storeAction = .store
        pass.colorAttachments[0].clearColor = MTLClearColor(red: 0, green: 0, blue: 0, alpha: 1)
        let cb = queue.makeCommandBuffer()!
        let enc = cb.makeRenderCommandEncoder(descriptor: pass)!
        enc.setRenderPipelineState(pipeline)
        let buffer = device.makeBuffer(bytes: &list, length: MemoryLayout<GPURect>.stride * list.count, options: .storageModeShared)!
        enc.setVertexBuffer(buffer, offset: 0, index: 0)
        enc.setVertexBytes(&viewport, length: MemoryLayout<SIMD2<Float>>.size, index: 1)
        enc.drawPrimitives(type: .triangleStrip, vertexStart: 0, vertexCount: 4, instanceCount: list.count)
        enc.endEncoding()
        let frame = n
        let targetUs = Int(target * 1_000_000)
        let commitUs = nowUs()
        var click = pendingClick
        pendingClick = nil
        click?["n"] = frame
        if warm {
            drawable.addPresentedHandler { d in
                log.write("warm", ["commit_us": commitUs, "presented_us": d.presentedTime > 0 ? Int(d.presentedTime * 1_000_000) : 0])
            }
            cb.present(drawable)
            cb.commit()
            return
        }
        drawable.addPresentedHandler { d in
            let presented = d.presentedTime > 0 ? Int(d.presentedTime * 1_000_000) : 0
            log.write("frame", ["n": frame, "target_us": targetUs, "commit_us": commitUs, "presented_us": presented])
            if var click {
                click["presented_us"] = presented
                log.write("click", click)
            }
        }
        cb.present(drawable)
        cb.commit()
        n += 1
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate {
    var window: NSWindow!

    func applicationDidFinishLaunching(_ notification: Notification) {
        let screens = NSScreen.screens
        let screen: NSScreen
        let id = { (s: NSScreen) in (s.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? NSNumber)?.uint32Value ?? 0 }
        switch options.screen {
        case "main": screen = screens[0]
        case "virtual":
            // LanKVM's virtual displays use vendor 0x4C4B ("LK").
            screen = screens.first { CGDisplayVendorNumber(id($0)) == 0x4C4B } ?? {
                FileHandle.standardError.write(Data("no LanKVM virtual display\n".utf8))
                exit(1)
            }()
        case let name where name.hasPrefix("display:"):
            // One display by its id: another LanKVM's virtual display may be there too.
            guard let wanted = UInt32(name.dropFirst("display:".count)), let match = screens.first(where: { id($0) == wanted }) else {
                FileHandle.standardError.write(Data("no display \(name.dropFirst("display:".count))\n".utf8))
                exit(1)
            }
            screen = match
        default:
            guard let i = Int(options.screen), screens.indices.contains(i) else {
                FileHandle.standardError.write(Data("no screen \(options.screen)\n".utf8))
                exit(1)
            }
            screen = screens[i]
        }
        let frame: NSRect
        if options.fullscreen {
            frame = screen.frame
            window = NSWindow(contentRect: frame, styleMask: [.borderless], backing: .buffered, defer: false, screen: screen)
            window.level = .statusBar
        } else {
            let f = options.frame
            frame = NSRect(x: screen.frame.minX + f.minX, y: screen.frame.maxY - f.minY - f.height, width: f.width, height: f.height)
            window = NSWindow(contentRect: frame, styleMask: [.titled, .closable, .resizable], backing: .buffered, defer: false, screen: screen)
            window.title = "LanKVM Frame Source"
        }
        // Shown on whatever Space is active, even over another app's full-screen Space, so the
        // test never depends on which Space was in front.
        window.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary, .stationary]
        if !options.fullscreen { window.level = .floating }
        window.setFrame(frame, display: false)
        window.isReleasedWhenClosed = false
        let view = SourceView(frame: NSRect(origin: .zero, size: frame.size))
        window.contentView = view
        switch options.overlay {
        case "tab":
            let tab = NSView(frame: NSRect(x: frame.width / 2 - 40, y: frame.height - 14, width: 80, height: 10))
            tab.wantsLayer = true
            tab.layer?.backgroundColor = NSColor.darkGray.cgColor
            tab.layer?.cornerRadius = 5
            view.addSubview(tab)
        case "pill":
            // Like the session control: a dark translucent capsule with a shadow.
            let pill = NSView(frame: NSRect(x: frame.width / 2 - 120, y: frame.height - 50, width: 240, height: 36))
            pill.wantsLayer = true
            pill.layer?.backgroundColor = NSColor.black.withAlphaComponent(0.72).cgColor
            pill.layer?.cornerRadius = 18
            pill.layer?.shadowColor = NSColor.black.cgColor
            pill.layer?.shadowOpacity = 0.35
            pill.layer?.shadowRadius = 8
            pill.layer?.shadowOffset = CGSize(width: 0, height: -2)
            view.addSubview(pill)
        case "alpha", "alphatext":
            let panel = NSView(frame: NSRect(x: frame.width / 2 - 150, y: frame.height - 94, width: 300, height: 90))
            panel.wantsLayer = true
            panel.layer?.backgroundColor = NSColor.black.withAlphaComponent(0.6).cgColor
            panel.layer?.cornerRadius = 12
            if options.overlay == "alphatext" {
                let label = NSTextField(labelWithString: "118 fps · 12.3 Mbit/s · RTT 0.3 ms")
                label.textColor = .white
                label.frame = NSRect(x: 12, y: 30, width: 280, height: 20)
                panel.addSubview(label)
                // Changes twice a second, like the stats overlay.
                Timer.scheduledTimer(withTimeInterval: 0.5, repeats: true) { _ in label.stringValue = "\(Int.random(in: 100...120)) fps · 12.3 Mbit/s · RTT 0.3 ms" }
            }
            view.addSubview(panel)
        case "glass", "hud":
            let size = options.overlay == "glass" ? NSSize(width: 80, height: 10) : NSSize(width: 300, height: 90)
            let glass = NSVisualEffectView(frame: NSRect(x: frame.width / 2 - size.width / 2, y: frame.height - size.height - 4, width: size.width, height: size.height))
            glass.material = .hudWindow
            glass.blendingMode = .withinWindow
            glass.state = .active
            glass.wantsLayer = true
            glass.layer?.cornerRadius = 5
            view.addSubview(glass)
        default: break
        }
        if options.background {
            window.orderFrontRegardless()
        } else {
            window.makeKeyAndOrderFront(nil)
            NSApp.activate(ignoringOtherApps: true)
        }
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { true }
}

let app = NSApplication.shared
app.setActivationPolicy(options.background ? .accessory : .regular)
let delegate = AppDelegate()
app.delegate = delegate
signal(SIGTERM) { _ in log.flush(); exit(0) }
app.run()
