import AppKit
import CLanKVM
import SwiftUI

/// The Display menu: which of the host's displays the session shows (its own screen, or one it
/// makes for this Mac at the size of a screen here or a custom size), and where a display made for
/// this Mac sits among the host's own. In the toolbar, the session control's menu and the Control
/// menu; checkmarks show what the session shows now.
struct DisplayMenu: View {
    @ObservedObject var session: SessionModel
    @ObservedObject var controls: SessionControlModel
    /// In the toolbar: the icon (its tooltip says the rest). In a menu: the title alone, like the
    /// items around it.
    var inToolbar = false

    var body: some View {
        Menu {
            if case .connected(let info) = session.phase {
                items(info)
            }
        } label: {
            if inToolbar {
                Label("Display", systemImage: "display")
            } else {
                Text("Display")
            }
        }
    }

    @ViewBuilder private func items(_ info: SessionInfo) -> some View {
        let host = "“\(info.hostName)”"
        let shown = info.display
        let here = controls.thisScreen ?? ScreenFit.main()
        // Screens the size of this one would only repeat it.
        let others = (controls.thisScreen == nil ? ScreenFit.all().filter { $0.id != here?.id } : controls.otherScreens)
            .filter { $0.size != here?.size }
        let fitted = ([here].compactMap { $0 } + others).contains { shown.sameSize(as: $0.display(shown.arrangement)) }
        let sameMac = info.displayAvailable == DisplayReason.sameMac || info.sameMachine
        let mayAdd = info.displayAvailable == DisplayReason.none || info.displayAvailable == DisplayReason.sameMac
        Section("Show \(host) at") {
            check("Its Own Screen", on: shown.kind == .main) { controls.showMainDisplay() }
            if let here {
                screenItem("This Screen’s Size", here, shown: shown, mayAdd: mayAdd)
            }
            ForEach(others) { screen in
                screenItem("“\(screen.name)” Size", screen, shown: shown, mayAdd: mayAdd)
            }
            check("Custom Size…", on: shown.kind == .virtual && !fitted) { controls.editCustomDisplay() }
                .disabled(!mayAdd)
        }
        if !mayAdd {
            Text(info.displayUnavailable.isEmpty ? "\(host) can't add a display for this Mac." : info.displayUnavailable)
        }
        Section("LanKVM Display") {
            let preferred = controls.preferredArrangement(info)
            ForEach(RemoteDisplay.Arrangement.allCases) { arrangement in
                check(Self.title(of: arrangement, host: host), on: arrangement == preferred) { controls.arrange(arrangement) }
                    .disabled(!mayAdd || (sameMac && arrangement != .extend))
            }
            if sameMac && mayAdd {
                Text("Not on this same Mac (test mode)")
            }
        }
    }

    /// A size for a display that fills a screen of this Mac; disabled when the host can't make it.
    private func screenItem(_ title: String, _ screen: ScreenFit, shown: RemoteDisplay, mayAdd: Bool) -> some View {
        let note = DisplayLimits.tooLarge(width: screen.width, height: screen.height) ? " · too large for LanKVM"
            : DisplayLimits.problem(width: screen.width, height: screen.height) != nil ? " · not a size LanKVM makes" : ""
        return check("\(title) · \(screen.sizeText)\(note)", on: shown.sameSize(as: screen.display(shown.arrangement))) {
            controls.showVirtualDisplay(screen)
        }
        .disabled(!mayAdd || !note.isEmpty)
    }

    /// A menu item with a checkmark when it's what the session shows. Choosing it again does
    /// nothing (CoreModel.showDisplay).
    private func check(_ title: String, on: Bool, action: @escaping () -> Void) -> some View {
        Toggle(title, isOn: Binding(get: { on }, set: { _ in action() }))
    }

    static func title(of arrangement: RemoteDisplay.Arrangement, host: String) -> String {
        switch arrangement {
        case .only: "Only It · \(host)’s screen shows a copy"
        case .main: "As Main Display · menu bar, Dock and new windows"
        case .extend: "Next to \(host)’s Screen · windows stay put"
        }
    }
}

/// Custom Size: a display of any size LanKVM makes, in pixels. A sheet, so typing goes to its
/// fields and never to the remote Mac (InputForwarder stops forwarding while a sheet is open).
struct CustomDisplaySheet: View {
    let hostName: String
    let apply: (RemoteDisplay) -> Void
    let cancel: () -> Void
    @State private var widthText: String
    @State private var heightText: String
    @State private var hidpi: Bool
    @State private var refreshHz: UInt32

    private static let refreshRates: [UInt32] = [30, 60, 120]

    init(draft: DisplayDraft, hostName: String, apply: @escaping (RemoteDisplay) -> Void, cancel: @escaping () -> Void) {
        self.hostName = hostName
        self.apply = apply
        self.cancel = cancel
        _widthText = State(initialValue: String(draft.width))
        _heightText = State(initialValue: String(draft.height))
        _hidpi = State(initialValue: draft.hidpi)
        // The fastest offered rate the screen it came from keeps up with.
        let most = min(draft.refreshHz, DisplayLimits.maxRefresh)
        _refreshHz = State(initialValue: Self.refreshRates.last { $0 <= most } ?? 30)
    }

    /// The size typed, rounded down to even numbers (displays are an even number of pixels each
    /// way); nil until both are numbers.
    private var size: (width: UInt32, height: UInt32)? {
        guard let width = UInt32(widthText.trimmingCharacters(in: .whitespaces)),
              let height = UInt32(heightText.trimmingCharacters(in: .whitespaces)) else { return nil }
        return (DisplayLimits.even(width), DisplayLimits.even(height))
    }

    private var problem: String? {
        guard let size else { return "Enter the width and the height in pixels." }
        return DisplayLimits.problem(width: size.width, height: size.height)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            VStack(alignment: .leading, spacing: 6) {
                Text("Custom Size")
                    .font(.system(size: 22, design: .serif))
                    .foregroundStyle(Color.lkText)
                Text("“\(hostName)” adds a display this size for this Mac, and this window shows it pixel for pixel.")
                    .font(.system(size: 13))
                    .foregroundStyle(Color.lkSecondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            VStack(alignment: .leading, spacing: 12) {
                row("Size") {
                    HStack(spacing: 8) {
                        TextField("Width", text: $widthText)
                            .frame(width: 76)
                            .accessibilityLabel("Width in pixels")
                        Text("×").foregroundStyle(Color.lkSecondary)
                        TextField("Height", text: $heightText)
                            .frame(width: 76)
                            .accessibilityLabel("Height in pixels")
                        Text("pixels").foregroundStyle(Color.lkSecondary)
                    }
                    .textFieldStyle(.roundedBorder)
                    .multilineTextAlignment(.trailing)
                }
                row(nil) {
                    Toggle("Retina (sharp text, everything at half size)", isOn: $hidpi)
                }
                row("Refresh") {
                    Picker("Refresh", selection: $refreshHz) {
                        ForEach(Self.refreshRates, id: \.self) { hz in
                            Text("\(hz) Hz").tag(hz)
                        }
                    }
                    .labelsHidden()
                    .pickerStyle(.segmented)
                    .fixedSize()
                }
            }
            .font(.system(size: 13))
            .foregroundStyle(Color.lkText)
            summary
            HStack(spacing: 10) {
                Spacer()
                Button("Cancel", action: cancel)
                    .buttonStyle(SecondaryButtonStyle())
                    .keyboardShortcut(.cancelAction)
                Button("Apply", action: submit)
                    .buttonStyle(PrimaryButtonStyle())
                    .keyboardShortcut(.defaultAction)
                    .disabled(problem != nil)
            }
        }
        .padding(24)
        .frame(width: 460)
        .background(Color.lkBackground)
    }

    /// A labelled line of the form, the labels right-aligned in a column of their own.
    private func row<Content: View>(_ label: String?, @ViewBuilder content: () -> Content) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 12) {
            Text(label ?? "")
                .frame(width: 56, alignment: .trailing)
            content()
        }
    }

    /// What it would look like and cost, or what's wrong with the size.
    @ViewBuilder private var summary: some View {
        if let problem {
            Label(problem, systemImage: "exclamationmark.triangle")
                .font(.system(size: 12))
                .foregroundStyle(Color.lkWarning)
        } else if let size {
            let display = RemoteDisplay(kind: .virtual, width: size.width, height: size.height, hidpi: hidpi, refreshHz: refreshHz)
            // Roughly what the stream takes at that size: at most 60 fps worth, as the host
            // spreads the same budget over more, smaller frames at 120.
            let mbps = min(max(Double(size.width) * Double(size.height) * Double(min(refreshHz, 60)) * 0.12 / 1_000_000, 8), 150)
            VStack(alignment: .leading, spacing: 4) {
                Text("Looks like \(display.looksLikeText) on “\(hostName)” · about \(Int(mbps.rounded())) Mbit/s at \(refreshHz) fps")
                if String(size.width) != widthText.trimmingCharacters(in: .whitespaces)
                    || String(size.height) != heightText.trimmingCharacters(in: .whitespaces) {
                    Text("Sizes are even: it will be \(display.sizeText).")
                }
            }
            .font(.system(size: 12))
            .foregroundStyle(Color.lkSecondary)
        }
    }

    private func submit() {
        guard problem == nil, let size else { return }
        apply(RemoteDisplay(kind: .virtual, width: size.width, height: size.height, hidpi: hidpi,
                            refreshHz: refreshHz))
    }
}

/// What the Custom Size sheet starts with (the display shown, or this screen's size).
struct DisplayDraft: Identifiable {
    let id = UUID()
    var width: UInt32
    var height: UInt32
    var hidpi: Bool
    var refreshHz: UInt32
}

/// A screen of this Mac as the display a host would make to fill it pixel for pixel: its size in
/// pixels without the notch's strip (a full-screen window stays below it), rounded down to even;
/// Retina when the screen is; and its refresh rate, at most 120 Hz.
struct ScreenFit: Identifiable, Equatable {
    let id: CGDirectDisplayID
    /// As macOS names it ("LG Ultrawide").
    let name: String
    let width: UInt32
    let height: UInt32
    let hidpi: Bool
    let refreshHz: UInt32

    init(_ screen: NSScreen) {
        id = screen.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? CGDirectDisplayID ?? 0
        name = screen.localizedName
        let scale = screen.backingScaleFactor
        let points = CGSize(width: screen.frame.width, height: screen.frame.height - screen.safeAreaInsets.top)
        width = DisplayLimits.even(UInt32(max(0, (points.width * scale).rounded(.down))))
        height = DisplayLimits.even(UInt32(max(0, (points.height * scale).rounded(.down))))
        hidpi = scale >= 2
        refreshHz = min(UInt32(max(30, screen.maximumFramesPerSecond)), DisplayLimits.maxRefresh)
    }

    var size: CGSize { CGSize(width: Int(width), height: Int(height)) }

    var sizeText: String { RemoteDisplay.sizeText(width, height) }

    /// The display to ask for, placed as `arrangement` says.
    func display(_ arrangement: RemoteDisplay.Arrangement) -> RemoteDisplay {
        RemoteDisplay(kind: .virtual, width: width, height: height, hidpi: hidpi, refreshHz: refreshHz, arrangement: arrangement)
    }

    /// Every screen attached to this Mac now.
    static func all() -> [ScreenFit] { NSScreen.screens.map(ScreenFit.init) }

    /// The screen with the key window (or the menu bar), for a session whose window isn't known.
    static func main() -> ScreenFit? { (NSScreen.main ?? NSScreen.screens.first).map(ScreenFit.init) }
}

/// Sizes a host makes (LK_DISPLAY_* in lankvm.h); it refuses others.
enum DisplayLimits {
    static let minWidth = UInt32(LK_DISPLAY_MIN_WIDTH)
    static let minHeight = UInt32(LK_DISPLAY_MIN_HEIGHT)
    static let maxSide = UInt32(LK_DISPLAY_MAX_SIDE)
    static let maxPixels = UInt64(LK_DISPLAY_MAX_PIXELS)
    static let maxAspect = UInt64(LK_DISPLAY_MAX_ASPECT)
    /// The most often a display refreshes, in Hz, whatever its size: the host re-encodes only the
    /// tiles of the picture that changed, several at once, and keeps the bit budget at 60 fps worth.
    static let maxRefresh: UInt32 = 120

    /// Why a host wouldn't make a display this size, in words; nil if it would.
    static func problem(width: UInt32, height: UInt32) -> String? {
        let (w, h) = (UInt64(width), UInt64(height))
        if width < minWidth || height < minHeight { return "At least \(minWidth) × \(minHeight) pixels." }
        if width > maxSide || height > maxSide { return "At most \(maxSide) pixels on a side." }
        if w * h > maxPixels { return "At most 35.4 million pixels, such as 8192 × 4320." }
        if max(w, h) > maxAspect * min(w, h) { return "At most four times as wide as it is tall, or as tall as it is wide." }
        if width % 2 != 0 || height % 2 != 0 { return "The width and the height must be even numbers." }
        return nil
    }

    /// More pixels, or a longer side, than LanKVM makes.
    static func tooLarge(width: UInt32, height: UInt32) -> Bool {
        width > maxSide || height > maxSide || UInt64(width) * UInt64(height) > maxPixels
    }

    /// Rounded down to even.
    static func even(_ value: UInt32) -> UInt32 { value & ~1 }
}

extension RemoteDisplay.Arrangement {
    /// LK_ARRANGE_* in lankvm.h.
    var code: UInt8 {
        let code = switch self {
        case .extend: LK_ARRANGE_EXTEND
        case .main: LK_ARRANGE_MAIN
        case .only: LK_ARRANGE_ONLY
        }
        return UInt8(code)
    }
}

/// The display last used with a host, put back on the next connection (`display.<hostId>` in the
/// defaults). Written only when the host shows a display the user picked; forgotten when the user
/// goes back to the host's own screen, or the host can't make displays. The host's own changes
/// never touch it.
struct RememberedDisplay {
    var display: RemoteDisplay
    /// The size of the screen of this Mac it was picked to fill: put back only while a screen that
    /// size is attached, otherwise offered.
    var matchedScreen: CGSize?

    init(display: RemoteDisplay, matchedScreen: CGSize?) {
        self.display = display
        self.matchedScreen = matchedScreen
    }

    init?(_ dictionary: [String: Any]) {
        guard dictionary["kind"] as? String == RemoteDisplay.Kind.virtual.rawValue,
              let width = (dictionary["width"] as? Int).flatMap(UInt32.init(exactly:)),
              let height = (dictionary["height"] as? Int).flatMap(UInt32.init(exactly:)),
              DisplayLimits.problem(width: width, height: height) == nil else { return nil }
        display = RemoteDisplay(
            kind: .virtual, width: width, height: height, hidpi: dictionary["hidpi"] as? Bool ?? false,
            refreshHz: (dictionary["refreshHz"] as? Int).flatMap(UInt32.init(exactly:)) ?? 60,
            arrangement: (dictionary["arrangement"] as? String).flatMap(RemoteDisplay.Arrangement.init) ?? .only)
        // "6144x2560".
        let sides = (dictionary["matchedScreen"] as? String)?.split(separator: "x").compactMap { Int($0) } ?? []
        matchedScreen = sides.count == 2 ? CGSize(width: sides[0], height: sides[1]) : nil
    }

    var dictionary: [String: Any] {
        var dictionary: [String: Any] = [
            "kind": display.kind.rawValue, "width": Int(display.width), "height": Int(display.height),
            "hidpi": display.hidpi, "refreshHz": Int(display.refreshHz), "arrangement": display.arrangement.rawValue,
        ]
        if let matchedScreen { dictionary["matchedScreen"] = "\(Int(matchedScreen.width))x\(Int(matchedScreen.height))" }
        return dictionary
    }

    private static func key(_ hostId: String) -> String { "display.\(hostId)" }

    static func load(_ hostId: String) -> RememberedDisplay? {
        guard !hostId.isEmpty, let saved = UserDefaults.standard.dictionary(forKey: key(hostId)) else { return nil }
        return RememberedDisplay(saved)
    }

    func save(_ hostId: String) {
        guard !hostId.isEmpty else { return }
        UserDefaults.standard.set(dictionary, forKey: Self.key(hostId))
    }

    static func forget(_ hostId: String) {
        guard !hostId.isEmpty else { return }
        UserDefaults.standard.removeObject(forKey: key(hostId))
    }
}
