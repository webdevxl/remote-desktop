import AppKit
import AVFoundation
import CoreMedia
import CoreVideo
import Darwin
import ImageIO
import QuartzCore
import ScreenCaptureKit

/// The screen scope of scripts/engine-bench.sh: with `LANKVM_SCOPE_LOG=<file>` set, this binary
/// films a viewer window's area of the screen instead of being LanKVM, reads Frame Source's frame
/// number strip in every captured frame, and logs when each number reached the glass.
///
/// It lives in the app only for LanKVM's Screen Recording grant (a separate tool would need a
/// grant of its own; a process started from a shell has none). In this mode the core never
/// starts: no network, no windows, no Dock icon. Launch it through LaunchServices so the grant
/// applies:
///
///     open -n -g -j target/release/LanKVM.app --env LANKVM_SCOPE_LOG=/abs/scope.jsonl --env LANKVM_SCOPE_PID=<pid>
///
/// Env: `LANKVM_SCOPE_PID` (pid of the window's owner) or `LANKVM_SCOPE_BUNDLE` (its bundle id);
/// `LANKVM_SCOPE_SECONDS` (10) to capture; `LANKVM_SCOPE_FPS` (240) the capture's rate limit;
/// `LANKVM_SCOPE_WAIT` (20) seconds to wait for the window; `LANKVM_SCOPE_STRIP=x,y,w,h` the
/// strip's grey frame in window points (skips detection); `LANKVM_SCOPE_DUMP=<file.png>` saves
/// the first captured frame (to see what the scope sees when it finds no strip).
///
/// The window is the owner's largest ordinary (layer 0) on-screen window, or its largest window
/// at any layer if it has no ordinary one (Frame Source's own floats: the scope's self-test). It
/// captures the display region under the window, not the window: what's really on the glass,
/// after the compositor. Times are µs of the mach clock (`CACurrentMediaTime`), the clock of
/// Frame Source's `commit_us`, so on one Mac the two logs join by frame number with no sync.
/// While the screen is locked the glass shows the lock screen, and the scope finds no strip.
///
/// `LANKVM_SCOPE_AVCAPTURE=<display id>` instead films a whole display the way Sunshine does on
/// macOS (AVCaptureScreenInput, BGRA at the display's pixel size, at most `LANKVM_SCOPE_FPS`), to
/// tell how much of Sunshine's latency its capture alone costs; `display_us` is then the sample's
/// presentation time (host clock).
///
/// The log (JSON lines): `start`, `window` (where it captures), `strip` (where it found the strip,
/// capture pixels), a `frame` line per readable frame {n, display_us, arrival_us}, an `unreadable`
/// line per frame it couldn't read once a strip was found, `error`, and `summary` last.
enum BenchScope {
    /// Runs the scope and exits when `LANKVM_SCOPE_LOG` is set; returns at once otherwise.
    static func runIfRequested() {
        let env = ProcessInfo.processInfo.environment
        guard let path = env["LANKVM_SCOPE_LOG"], !path.isEmpty else { return }
        if let id = env["LANKVM_SCOPE_AVCAPTURE"].flatMap({ CGDirectDisplayID($0) }) {
            AVScope(logPath: path, displayID: id).run()
        }
        Scope(logPath: path).run()
    }
}

/// JSON lines, written on a queue of their own so the capture callback never waits on the disk.
private final class ScopeLog: @unchecked Sendable {
    private let handle: FileHandle?
    private let queue = DispatchQueue(label: "dev.lankvm.scope.log")

    init(path: String) {
        FileManager.default.createFile(atPath: path, contents: nil)
        handle = FileHandle(forWritingAtPath: path)
    }

    var isOpen: Bool { handle != nil }

    func write(_ type: String, _ fields: [String: Any]) {
        var object = fields
        object["type"] = type
        guard let data = try? JSONSerialization.data(withJSONObject: object, options: [.sortedKeys]) else { return }
        line(data)
    }

    /// A line formatted by the caller: frame lines come up to 240 a second.
    func raw(_ text: String) { line(Data(text.utf8)) }

    private func line(_ data: Data) {
        var data = data
        data.append(0x0A)
        queue.async { [handle] in handle?.write(data) }
    }

    func flush() { queue.sync {} }
}

private let timebase: mach_timebase_info_data_t = {
    var info = mach_timebase_info_data_t()
    mach_timebase_info(&info)
    return info
}()

/// Mach ticks to µs (on Apple silicon a tick is 125/3 ns, not 1 ns).
private func microseconds(ticks: UInt64) -> Int {
    Int(ticks * UInt64(timebase.numer) / UInt64(timebase.denom) / 1000)
}

private func nowUs() -> Int { Int(CACurrentMediaTime() * 1_000_000) }

/// While the screen is locked windows don't draw, and the scope would film nothing.
private func screenLocked() -> Bool {
    let session = CGSessionCopyCurrentDictionary() as? [String: Any]
    return session?["CGSSessionScreenIsLocked"] as? Bool ?? false
}

/// Frame Source's strip, as captured: the top-left corner of its grey frame and the scale, in
/// capture pixels per strip point. The frame is 520×40 points around 16 squares of 32 points.
private struct Strip {
    var x: Double
    var y: Double
    var s: Double
}

/// A locked BGRA capture.
private struct Pixels {
    let base: UnsafePointer<UInt8>
    let bytesPerRow: Int
    let width: Int
    let height: Int

    /// Luma × 1000 (Y = 0.299 R + 0.587 G + 0.114 B).
    @inline(__always) func luma(_ x: Int, _ y: Int) -> Int {
        let p = base + y * bytesPerRow + x * 4
        return 114 * Int(p[0]) + 587 * Int(p[1]) + 299 * Int(p[2])
    }

    /// The strip's grey frame (RGB 0.5, Y 128 ± 30, which scaling and video coding keep).
    @inline(__always) func isGrey(_ x: Int, _ y: Int) -> Bool {
        let v = luma(x, y)
        return v >= 98_000 && v <= 158_000
    }

    /// Share of grey pixels on column `x` from row `y0` to `y1`, or on row `y` from `x0` to `x1`.
    func greyShare(column x: Int, from y0: Int, to y1: Int) -> Double {
        guard x >= 0, x < width, y0 >= 0, y1 < height, y1 >= y0 else { return 0 }
        var grey = 0
        for y in y0...y1 where isGrey(x, y) { grey += 1 }
        return Double(grey) / Double(y1 - y0 + 1)
    }

    func greyShare(row y: Int, from x0: Int, to x1: Int) -> Double {
        guard y >= 0, y < height, x0 >= 0, x1 < width, x1 >= x0 else { return 0 }
        var grey = 0
        for x in x0...x1 where isGrey(x, y) { grey += 1 }
        return Double(grey) / Double(x1 - x0 + 1)
    }

    /// Mean luma (0-255) of a 7×7 grid over the box centred at (cx, cy), `half` pixels each way.
    func meanLuma(cx: Double, cy: Double, half: Double) -> Double? {
        var sum = 0, count = 0
        for j in 0..<7 {
            let y = Int((cy + half * (Double(j) / 3 - 1)).rounded())
            guard y >= 0, y < height else { return nil }
            for i in 0..<7 {
                let x = Int((cx + half * (Double(i) / 3 - 1)).rounded())
                guard x >= 0, x < width else { return nil }
                sum += luma(x, y)
                count += 1
            }
        }
        return Double(sum) / Double(count) / 1000
    }
}

/// The frame number on the strip (n modulo 65536), or nil unless all 16 squares read clearly.
private func readStrip(_ px: Pixels, _ strip: Strip) -> Int? {
    var gray = 0
    for i in 0..<16 {
        // Square i spans 4 + 32i ..< 36 + 32i points of the frame; its inner half is read.
        let cx = strip.x + (20 + 32 * Double(i)) * strip.s
        let cy = strip.y + 20 * strip.s
        guard let y = px.meanLuma(cx: cx, cy: cy, half: 8 * strip.s) else { return nil }
        if y >= 170 {
            gray |= 1 << i
        } else if y > 85 {
            return nil
        }
    }
    var n = gray
    n ^= n >> 1
    n ^= n >> 2
    n ^= n >> 4
    n ^= n >> 8
    return n & 0xFFFF
}

/// Finds the strip in the top half of a capture: a horizontal grey run of at least 120 px (the
/// frame's top border, 520 points), checked against the frame's geometry and an all-readable
/// number. The viewer shows the source scaled and letterboxed, so the scale comes from the run.
private func detectStrip(_ px: Pixels) -> Strip? {
    let minRun = 120
    // A run of 120 px holds at least 3 of every 32nd pixel: only those are tested first.
    let step = 32
    for y in 0..<(px.height / 2) {
        var x = 0
        while x < px.width {
            guard px.isGrey(x, y) else {
                x += step
                continue
            }
            var a = x, b = x
            while a > 0 && px.isGrey(a - 1, y) { a -= 1 }
            while b + 1 < px.width && px.isGrey(b + 1, y) { b += 1 }
            if b - a + 1 >= minRun, let strip = checkStrip(px, x0: a, y0: y, length: b - a + 1) {
                return strip
            }
            x = b + 1
        }
    }
    return nil
}

/// A run at (x0, y0) of `length` px as the top border of the strip's frame: the side borders run
/// down its height (40/520 of its length), the bottom border runs across, and all 16 squares read.
private func checkStrip(_ px: Pixels, x0: Int, y0: Int, length: Int) -> Strip? {
    var s = Double(length) / 520
    let bottom = y0 + Int(40 * s) - 1
    guard bottom < px.height else { return nil }
    // A border is 4 points: at small scales only one or two of its pixels aren't blends with the
    // squares or the background, so any line inside it (give or take a pixel) counts.
    let border = max(1, Int((4 * s).rounded()))
    func column(_ x: Int) -> Bool { px.greyShare(column: x, from: y0 + 1, to: bottom - 1) >= 0.9 }
    guard ((x0 - 1)...(x0 + border)).contains(where: column),
          ((x0 + length - 1 - border)...(x0 + length)).contains(where: column)
    else { return nil }
    let lastRow = min(bottom + 1, px.height - 1)
    guard (min(y0 + Int(36 * s), lastRow)...lastRow).contains(where: { px.greyShare(row: $0, from: x0 + border, to: x0 + length - 1 - border) >= 0.9 })
    else { return nil }
    // Measure again along the top border's middle row: the first row found may be a blend.
    var x = x0
    let row = y0 + (border - 1) / 2
    let mid = x0 + length / 2
    if px.isGrey(mid, row) {
        var a = mid, b = mid
        while a > 0 && px.isGrey(a - 1, row) { a -= 1 }
        while b + 1 < px.width && px.isGrey(b + 1, row) { b += 1 }
        if abs((b - a + 1) - length) <= max(4, length / 50) {
            x = a
            s = Double(b - a + 1) / 520
        }
    }
    let strip = Strip(x: Double(x), y: Double(y0), s: s)
    return readStrip(px, strip) == nil ? nil : strip
}

private final class Scope: NSObject, SCStreamOutput, SCStreamDelegate, @unchecked Sendable {
    /// Where to capture: the window, the display it's on, the window's rectangle in that display's
    /// points (top-left origin, clipped to the display) and the display's backing scale.
    struct Target {
        let window: SCWindow
        let display: SCDisplay
        let rect: CGRect
        let scale: Double

        func same(as other: Target) -> Bool {
            display.displayID == other.display.displayID && rect == other.rect && scale == other.scale
        }
    }

    let log: ScopeLog
    let pid: pid_t?
    let bundle: String?
    let seconds: Double
    let fps: Int
    let wait: Double
    /// `LANKVM_SCOPE_STRIP`: the grey frame in window points.
    let manualStrip: CGRect?
    /// `LANKVM_SCOPE_DUMP`: where the first captured frame goes, as a PNG.
    var dump: String?

    // Everything below is used on `queue` only (the capture callback runs there too).
    let queue = DispatchQueue(label: "dev.lankvm.scope", qos: .userInteractive)
    var stream: SCStream?
    var target: Target?
    var strip: Strip?
    var signals: [DispatchSourceSignal] = []
    var started = 0.0
    /// Wall-clock time the capture started, for lining up with cpu.log.
    var startedEpoch = 0.0
    var finished = false
    /// Consecutive unreadable frames; at 30 the strip is looked for again.
    var misses = 0
    var lastLookup = 0.0
    var looking = false
    var everFound = false
    var frames = 0, readable = 0, unreadable = 0, idle = 0, blank = 0, undetected = 0, detections = 0
    var seen = Set<Int>()

    init(logPath: String) {
        let env = ProcessInfo.processInfo.environment
        log = ScopeLog(path: logPath)
        pid = env["LANKVM_SCOPE_PID"].flatMap { pid_t($0) }
        bundle = env["LANKVM_SCOPE_BUNDLE"].flatMap { $0.isEmpty ? nil : $0 }
        seconds = env["LANKVM_SCOPE_SECONDS"].flatMap { Double($0) } ?? 10
        fps = max(1, env["LANKVM_SCOPE_FPS"].flatMap { Int($0) } ?? 240)
        wait = env["LANKVM_SCOPE_WAIT"].flatMap { Double($0) } ?? 20
        let p = (env["LANKVM_SCOPE_STRIP"] ?? "").split(separator: ",").compactMap { Double($0) }
        manualStrip = p.count == 4 && p[2] > 0 ? CGRect(x: p[0], y: p[1], width: p[2], height: p[3]) : nil
        dump = env["LANKVM_SCOPE_DUMP"].flatMap { $0.isEmpty ? nil : $0 }
    }

    func run() -> Never {
        // No Dock icon, no menu bar, never in front.
        NSApplication.shared.setActivationPolicy(.prohibited)
        guard log.isOpen else { exit(2) }
        for sig in [SIGTERM, SIGINT] {
            signal(sig, SIG_IGN)
            let source = DispatchSource.makeSignalSource(signal: sig, queue: queue)
            source.setEventHandler { [self] in finish(0) }
            source.resume()
            signals.append(source)
        }
        var start: [String: Any] = ["pid": Int(getpid()), "seconds": seconds, "fps": fps, "wait": wait, "time_us": nowUs(),
                                    "epoch": Date().timeIntervalSince1970, "locked": screenLocked()]
        if let pid { start["target_pid"] = Int(pid) }
        if let bundle { start["target_bundle"] = bundle }
        log.write("start", start)
        queue.async { [self] in
            guard pid != nil || bundle != nil else {
                finish(2, error: "set LANKVM_SCOPE_PID or LANKVM_SCOPE_BUNDLE to the viewer window's owner")
                return
            }
            lookUp(until: CACurrentMediaTime() + wait)
        }
        // The capture runs on `queue`; the main thread only keeps the process alive.
        while true { CFRunLoopRun() }
    }

    // MARK: Finding the window

    /// The largest ordinary on-screen window of the owner, and the display it's mostly on.
    func findTarget(_ done: @escaping (Target?, Error?) -> Void) {
        SCShareableContent.getExcludingDesktopWindows(false, onScreenWindowsOnly: true) { [self] content, error in
            queue.async { [self] in
                guard let content else { return done(nil, error) }
                let owned = content.windows.filter { w in
                    guard w.frame.width >= 64, w.frame.height >= 64, let app = w.owningApplication else { return false }
                    if let pid { return app.processID == pid }
                    return app.bundleIdentifier == bundle
                }
                // An ordinary window (layer 0) if it has one: viewers do. Frame Source's own window
                // (the scope's self-test) floats.
                let ordinary = owned.filter { $0.windowLayer == 0 }
                guard let window = (ordinary.isEmpty ? owned : ordinary).max(by: { $0.frame.width * $0.frame.height < $1.frame.width * $1.frame.height }) else {
                    return done(nil, nil)
                }
                func overlap(_ d: SCDisplay) -> CGFloat {
                    let r = d.frame.intersection(window.frame)
                    return r.isNull ? 0 : r.width * r.height
                }
                guard let display = content.displays.max(by: { overlap($0) < overlap($1) }), overlap(display) > 0 else {
                    return done(nil, nil)
                }
                let onDisplay = window.frame.intersection(display.frame)
                let rect = onDisplay.offsetBy(dx: -display.frame.minX, dy: -display.frame.minY).integral
                var scale = 2.0
                if let mode = CGDisplayCopyDisplayMode(display.displayID), mode.width > 0 {
                    scale = Double(mode.pixelWidth) / Double(mode.width)
                }
                done(Target(window: window, display: display, rect: rect, scale: scale), nil)
            }
        }
    }

    func lookUp(until deadline: Double) {
        findTarget { [self] target, error in
            if let error {
                return finish(1, error: "can't list windows (\(error.localizedDescription)); LanKVM needs Screen Recording here, and the scope must be launched with open")
            }
            if let target { return start(target) }
            guard CACurrentMediaTime() < deadline else {
                let owner = pid.map { "pid \($0)" } ?? "bundle \(bundle ?? "")"
                let locked = screenLocked() ? " (the screen is locked: windows don't draw)" : ""
                return finish(1, error: "no on-screen window of \(owner) after \(Int(wait)) s\(locked)")
            }
            queue.asyncAfter(deadline: .now() + 0.25) { [self] in lookUp(until: deadline) }
        }
    }

    func configuration(for target: Target) -> SCStreamConfiguration {
        let config = SCStreamConfiguration()
        config.sourceRect = target.rect
        config.width = max(1, Int((target.rect.width * target.scale).rounded()))
        config.height = max(1, Int((target.rect.height * target.scale).rounded()))
        config.pixelFormat = kCVPixelFormatType_32BGRA
        config.showsCursor = false
        config.queueDepth = 6
        config.minimumFrameInterval = CMTime(value: 1, timescale: CMTimeScale(fps))
        return config
    }

    func logWindow(_ target: Target) {
        let f = target.window.frame
        log.write("window", [
            "pid": Int(target.window.owningApplication?.processID ?? 0),
            "bundle": target.window.owningApplication?.bundleIdentifier ?? "",
            "title": target.window.title ?? "",
            "layer": target.window.windowLayer,
            "x": Double(f.minX), "y": Double(f.minY), "w": Double(f.width), "h": Double(f.height),
            "display_id": Int(target.display.displayID),
            "scale": target.scale,
            "rect": [Double(target.rect.minX), Double(target.rect.minY), Double(target.rect.width), Double(target.rect.height)],
            "capture": [Int((target.rect.width * target.scale).rounded()), Int((target.rect.height * target.scale).rounded())],
        ])
    }

    /// The manual strip in capture pixels (the capture starts where the window does, unless the
    /// window sticks out of the display's top or left edge).
    func manual(_ target: Target) -> Strip? {
        guard let m = manualStrip else { return nil }
        let window = target.window.frame
        let clipX = target.rect.minX + target.display.frame.minX - window.minX
        let clipY = target.rect.minY + target.display.frame.minY - window.minY
        return Strip(x: (m.minX - clipX) * target.scale, y: (m.minY - clipY) * target.scale, s: m.width * target.scale / 520)
    }

    func start(_ target: Target) {
        self.target = target
        let stream = SCStream(filter: SCContentFilter(display: target.display, excludingWindows: []),
                              configuration: configuration(for: target), delegate: self)
        do {
            try stream.addStreamOutput(self, type: .screen, sampleHandlerQueue: queue)
        } catch {
            return finish(1, error: "can't add the capture output: \(error.localizedDescription)")
        }
        self.stream = stream
        logWindow(target)
        if let m = manual(target) { found(m) }
        stream.startCapture { [self] error in
            queue.async { [self] in
                if let error {
                    return finish(1, error: "can't capture: \(error.localizedDescription); LanKVM needs Screen Recording here")
                }
                started = CACurrentMediaTime()
                startedEpoch = Date().timeIntervalSince1970
                queue.asyncAfter(deadline: .now() + seconds) { [self] in finish(0) }
            }
        }
    }

    /// The window again, after the strip went missing: it may have moved or changed size.
    func lookAgain() {
        guard !looking, let stream, let old = target else { return }
        looking = true
        lastLookup = CACurrentMediaTime()
        findTarget { [self] new, _ in
            guard let new, !new.same(as: old) else {
                looking = false
                return
            }
            let sameDisplay = new.display.displayID == old.display.displayID
            Task {
                do {
                    if !sameDisplay {
                        try await stream.updateContentFilter(SCContentFilter(display: new.display, excludingWindows: []))
                    }
                    try await stream.updateConfiguration(configuration(for: new))
                } catch {
                    queue.async { [self] in log.write("error", ["message": "can't follow the window: \(error.localizedDescription)"]) }
                }
                queue.async { [self] in
                    target = new
                    strip = manual(new)
                    misses = 0
                    looking = false
                    logWindow(new)
                }
            }
        }
    }

    // MARK: Frames

    func found(_ s: Strip) {
        strip = s
        everFound = true
        detections += 1
        misses = 0
        log.write("strip", ["x": s.x, "y": s.y, "s": s.s, "manual": manualStrip != nil])
    }

    func stream(_ stream: SCStream, didOutputSampleBuffer sampleBuffer: CMSampleBuffer, of type: SCStreamOutputType) {
        let arrival = nowUs()
        guard type == .screen, !finished,
              let attachments = CMSampleBufferGetSampleAttachmentsArray(sampleBuffer, createIfNecessary: false) as? [[SCStreamFrameInfo: Any]],
              let info = attachments.first,
              let raw = info[.status] as? Int, let status = SCFrameStatus(rawValue: raw)
        else { return }
        switch status {
        case .complete: break
        case .idle: idle += 1; return
        case .blank: blank += 1; return
        default: return
        }
        frames += 1
        let ticks = (info[.displayTime] as? NSNumber)?.uint64Value ?? 0
        let display = ticks > 0 ? microseconds(ticks: ticks) : 0
        guard let image = CMSampleBufferGetImageBuffer(sampleBuffer) else { return }
        CVPixelBufferLockBaseAddress(image, .readOnly)
        defer { CVPixelBufferUnlockBaseAddress(image, .readOnly) }
        guard let base = CVPixelBufferGetBaseAddress(image) else { return }
        let px = Pixels(base: UnsafePointer(base.assumingMemoryBound(to: UInt8.self)), bytesPerRow: CVPixelBufferGetBytesPerRow(image),
                        width: CVPixelBufferGetWidth(image), height: CVPixelBufferGetHeight(image))
        if let path = dump {
            dump = nil
            save(px, to: path)
        }

        if strip == nil, manualStrip == nil, let s = detectStrip(px) { found(s) }
        guard let current = strip else {
            // Frame Source starts after the scope: until its strip shows, nothing is missed.
            if everFound { miss(display) } else { undetected += 1 }
            if CACurrentMediaTime() - lastLookup > 2 { lookAgain() }
            return
        }
        guard let n = readStrip(px, current) else { return miss(display) }
        readable += 1
        misses = 0
        seen.insert(n)
        log.raw("{\"arrival_us\":\(arrival),\"display_us\":\(display),\"n\":\(n),\"type\":\"frame\"}")
    }

    /// A capture as a PNG (BGRA, alpha ignored).
    func save(_ px: Pixels, to path: String) {
        let info = CGImageAlphaInfo.noneSkipFirst.rawValue | CGBitmapInfo.byteOrder32Little.rawValue
        guard let context = CGContext(data: UnsafeMutableRawPointer(mutating: px.base), width: px.width, height: px.height,
                                      bitsPerComponent: 8, bytesPerRow: px.bytesPerRow, space: CGColorSpaceCreateDeviceRGB(), bitmapInfo: info),
              let image = context.makeImage(),
              let file = CGImageDestinationCreateWithURL(URL(fileURLWithPath: path) as CFURL, "public.png" as CFString, 1, nil)
        else { return log.write("error", ["message": "can't save \(path)"]) }
        CGImageDestinationAddImage(file, image, nil)
        if !CGImageDestinationFinalize(file) { log.write("error", ["message": "can't save \(path)"]) }
    }

    func miss(_ display: Int) {
        unreadable += 1
        log.raw("{\"display_us\":\(display),\"type\":\"unreadable\"}")
        misses += 1
        if misses >= 30 && manualStrip == nil {
            // Moved, resized or covered: look for the strip (and the window) again.
            strip = nil
            misses = 0
            lookAgain()
        }
    }

    func stream(_ stream: SCStream, didStopWithError error: Error) {
        queue.async { [self] in finish(1, error: "capture stopped: \(error.localizedDescription)") }
    }

    /// Writes the summary and quits. On `queue`.
    func finish(_ code: Int32, error: String? = nil) {
        guard !finished else { return }
        finished = true
        if let error { log.write("error", ["message": error]) }
        log.write("summary", [
            "frames": frames, "readable": readable, "unreadable": unreadable, "idle": idle, "blank": blank,
            "undetected": undetected, "detections": detections, "distinct": seen.count,
            "seconds": started > 0 ? CACurrentMediaTime() - started : 0, "capture_epoch": startedEpoch,
        ])
        log.flush()
        if let stream {
            // Tidy, but never wait long for it.
            let stopped = DispatchSemaphore(value: 0)
            stream.stopCapture { _ in stopped.signal() }
            _ = stopped.wait(timeout: .now() + 1)
        }
        exit(code)
    }
}

/// Sunshine's capture, for comparison (`LANKVM_SCOPE_AVCAPTURE`): AVCaptureScreenInput on a whole
/// display, set up as Sunshine's src/platform/macos/av_video.m does (minimum frame duration 1/fps,
/// BGRA at the display's pixel size, aspect-fit scaling, a serial queue at user-initiated QoS).
private final class AVScope: NSObject, AVCaptureVideoDataOutputSampleBufferDelegate, @unchecked Sendable {
    let log: ScopeLog
    let displayID: CGDirectDisplayID
    let seconds: Double
    let fps: Int
    let queue = DispatchQueue(label: "dev.lankvm.scope.av", qos: .userInitiated)
    let session = AVCaptureSession()
    var signals: [DispatchSourceSignal] = []
    var strip: Strip?
    var started = 0.0
    var finished = false
    var frames = 0, readable = 0, unreadable = 0, undetected = 0, detections = 0
    var seen = Set<Int>()

    init(logPath: String, displayID: CGDirectDisplayID) {
        let env = ProcessInfo.processInfo.environment
        log = ScopeLog(path: logPath)
        self.displayID = displayID
        seconds = env["LANKVM_SCOPE_SECONDS"].flatMap { Double($0) } ?? 10
        fps = max(1, env["LANKVM_SCOPE_FPS"].flatMap { Int($0) } ?? 120)
    }

    func run() -> Never {
        NSApplication.shared.setActivationPolicy(.prohibited)
        guard log.isOpen else { exit(2) }
        for sig in [SIGTERM, SIGINT] {
            signal(sig, SIG_IGN)
            let source = DispatchSource.makeSignalSource(signal: sig, queue: queue)
            source.setEventHandler { [self] in finish(0) }
            source.resume()
            signals.append(source)
        }
        log.write("start", ["pid": Int(getpid()), "backend": "avcapture", "display_id": Int(displayID), "seconds": seconds, "fps": fps,
                            "time_us": nowUs(), "epoch": Date().timeIntervalSince1970, "locked": screenLocked()])
        guard let mode = CGDisplayCopyDisplayMode(displayID), let input = AVCaptureScreenInput(displayID: displayID) else {
            log.write("error", ["message": "no display \(displayID)"])
            log.flush()
            exit(1)
        }
        input.minFrameDuration = CMTime(value: 1, timescale: CMTimeScale(fps))
        let output = AVCaptureVideoDataOutput()
        output.videoSettings = [
            kCVPixelBufferPixelFormatTypeKey as String: kCVPixelFormatType_32BGRA,
            kCVPixelBufferWidthKey as String: mode.pixelWidth,
            kCVPixelBufferHeightKey as String: mode.pixelHeight,
            AVVideoScalingModeKey: AVVideoScalingModeResizeAspect,
        ]
        output.setSampleBufferDelegate(self, queue: queue)
        guard session.canAddInput(input), session.canAddOutput(output) else {
            log.write("error", ["message": "can't set up AVCaptureSession for display \(displayID)"])
            log.flush()
            exit(1)
        }
        session.addInput(input)
        session.addOutput(output)
        log.write("window", ["display_id": Int(displayID), "capture": [mode.pixelWidth, mode.pixelHeight], "backend": "avcapture"])
        session.startRunning()
        queue.async { [self] in
            started = CACurrentMediaTime()
            queue.asyncAfter(deadline: .now() + seconds) { [self] in finish(0) }
        }
        while true { CFRunLoopRun() }
    }

    func captureOutput(_ output: AVCaptureOutput, didOutput sampleBuffer: CMSampleBuffer, from connection: AVCaptureConnection) {
        let arrival = nowUs()
        guard !finished, let image = CMSampleBufferGetImageBuffer(sampleBuffer) else { return }
        frames += 1
        let pts = CMSampleBufferGetPresentationTimeStamp(sampleBuffer)
        let display = pts.isValid ? Int(CMTimeGetSeconds(pts) * 1_000_000) : 0
        CVPixelBufferLockBaseAddress(image, .readOnly)
        defer { CVPixelBufferUnlockBaseAddress(image, .readOnly) }
        guard let base = CVPixelBufferGetBaseAddress(image) else { return }
        let px = Pixels(base: UnsafePointer(base.assumingMemoryBound(to: UInt8.self)), bytesPerRow: CVPixelBufferGetBytesPerRow(image),
                        width: CVPixelBufferGetWidth(image), height: CVPixelBufferGetHeight(image))
        if strip == nil, let s = detectStrip(px) {
            strip = s
            detections += 1
            log.write("strip", ["x": s.x, "y": s.y, "s": s.s, "manual": false])
        }
        guard let current = strip else { undetected += 1; return }
        guard let n = readStrip(px, current) else {
            unreadable += 1
            log.raw("{\"display_us\":\(display),\"type\":\"unreadable\"}")
            return
        }
        readable += 1
        seen.insert(n)
        log.raw("{\"arrival_us\":\(arrival),\"display_us\":\(display),\"n\":\(n),\"type\":\"frame\"}")
    }

    func finish(_ code: Int32) {
        guard !finished else { return }
        finished = true
        log.write("summary", ["frames": frames, "readable": readable, "unreadable": unreadable, "idle": 0, "blank": 0,
                              "undetected": undetected, "detections": detections, "distinct": seen.count,
                              "seconds": started > 0 ? CACurrentMediaTime() - started : 0])
        log.flush()
        session.stopRunning()
        exit(code)
    }
}
