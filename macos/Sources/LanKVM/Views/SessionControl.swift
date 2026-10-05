import Accessibility
import AppKit
import CLanKVM
import Combine
import SwiftUI

/// Something to do on the remote Mac as a whole, from the session control's menu or the Control
/// menu. The host runs it itself: keystrokes such as ⌃↑ may be remapped or turned off there.
enum RemoteAction: CaseIterable, Identifiable {
    case missionControl, appExpose, showDesktop, previousSpace, nextSpace

    var id: Self { self }

    var title: String {
        switch self {
        case .missionControl: "Mission Control"
        case .appExpose: "App Exposé"
        case .showDesktop: "Show Desktop"
        case .previousSpace: "Move Left a Space"
        case .nextSpace: "Move Right a Space"
        }
    }

    /// LK_SYSTEM_* in lankvm.h.
    var code: UInt16 {
        let code = switch self {
        case .missionControl: LK_SYSTEM_MISSION_CONTROL
        case .appExpose: LK_SYSTEM_APP_EXPOSE
        case .showDesktop: LK_SYSTEM_SHOW_DESKTOP
        case .previousSpace: LK_SYSTEM_PREVIOUS_SPACE
        case .nextSpace: LK_SYSTEM_NEXT_SPACE
        }
        return UInt16(code)
    }
}

/// State and actions behind the session control, the small floating control over the remote
/// screen (and behind the Control menu while its window is key). One per session.
@MainActor
final class SessionControlModel: ObservableObject {
    /// User settings: whether the control is shown at all, and whether it stays expanded.
    static let visibleKey = "showSessionControl"
    static let pinnedKey = "sessionControlPinned"
    /// How often it has opened by itself when control started in a window (not full screen):
    /// only the first few times, after that the window's subtitle says enough.
    private static let windowedFlashesKey = "sessionControlFlashes"
    private static let windowedFlashLimit = 3
    /// Within this distance the collapsed control brightens, and it fades again only after the
    /// pointer has been away this long.
    private static let peekDistance: CGFloat = 80
    private static let peekLinger: TimeInterval = 3
    /// Around the control, the mouse still belongs to it (not sent to the remote Mac).
    private static let exclusionMargin: CGFloat = 4

    enum State: Equatable { case viewing, refused, requesting, forwarding, released, paused }

    /// Input goes to the remote Mac now (InputForwarder.isForwarding).
    @Published private(set) var isForwarding = false
    /// Still controlling, but this Mac's keyboard and mouse were given back (Release).
    @Published private(set) var isReleased = false
    @Published private(set) var isFullScreen = false
    /// Expanded for a moment to show what just happened (control started, released).
    @Published private(set) var isFlashing = false
    /// The pointer is close to the control, so it shows at full strength.
    @Published private(set) var isPointerNear = false
    /// Where it's docked, remembered per remote Mac.
    @Published private(set) var placement = Placement.default
    /// The remote screen's size, and the expanded control's: other overlays keep clear of where
    /// it is or may expand to.
    @Published private(set) var screenSize = CGSize.zero
    @Published private(set) var expandedSize = CGSize(width: 340, height: SessionControl.pillHeight)
    /// This window's screen, and this Mac's other screens, as displays the host could make to fill
    /// them (the Display menu). Kept current as the window moves and screens come and go.
    @Published private(set) var thisScreen: ScreenFit?
    @Published private(set) var otherScreens: [ScreenFit] = []
    /// Set to open the Custom Size sheet over the window (from the Display menu, wherever it is).
    @Published var customDisplay: DisplayDraft?
    /// The arrangement picked while the session shows the host's own screen: the next display
    /// picked goes there.
    @Published var nextArrangement: RemoteDisplay.Arrangement?

    /// The control's frame on the remote screen (points, top-left origin), null while hidden.
    private(set) var frame = CGRect.null

    private weak var session: SessionModel?
    private weak var forwarder: InputForwarder?
    private weak var window: NSWindow?
    private var windowObservers: [NSObjectProtocol] = []
    private var pendingInput: (forwarding: Bool, released: Bool)?
    /// Forwarding started since control did: later starts (the focus coming back, Resume, another
    /// Space) aren't control starting.
    private var forwardedSinceControlStarted = false
    private var controlWatch: AnyCancellable?
    private var flashGeneration = 0
    /// Bumped whenever the pointer comes near, which cancels a pending fade.
    private var peekGeneration = 0
    private var fadePendingFor: Int?

    init(session: SessionModel) {
        self.session = session
        // Viewing again, refused, or the session over: the next grant starts control afresh.
        controlWatch = session.$control.sink { [weak self] control in
            guard control != .active else { return }
            MainActor.assumeIsolated { self?.forwardedSinceControlStarted = false }
        }
    }

    deinit {
        windowObservers.forEach(NotificationCenter.default.removeObserver)
    }

    /// What the control shows. Views that read it observe the session too.
    var state: State {
        switch session?.control ?? .off {
        case .off: .viewing
        case .refused: .refused
        case .requesting: .requesting
        case .active: isForwarding ? .forwarding : isReleased ? .released : .paused
        }
    }

    /// Shown while controlling or asking to, and while viewing only in full screen, where the
    /// toolbar is hidden. Never while connecting, asking for the PIN, or after the session ended.
    func isShown(enabled: Bool) -> Bool {
        guard enabled, case .connected = session?.phase else { return false }
        switch state {
        case .viewing, .refused: return isFullScreen
        case .requesting, .forwarding, .released, .paused: return true
        }
    }

    // MARK: Actions

    func release() {
        forwarder?.release()
    }

    func resume() {
        // Paused because the window is in the background: bringing it forward resumes.
        if let window, !window.isKeyWindow {
            NSApp.activate(ignoringOtherApps: true)
            window.makeKeyAndOrderFront(nil)
        }
        forwarder?.resume()
    }

    func requestControl() {
        if let session { CoreModel.shared.setControl(true, for: session.id) }
    }

    func viewOnly() {
        if let session { CoreModel.shared.setControl(false, for: session.id) }
    }

    func perform(_ action: RemoteAction) {
        forwarder?.perform(action)
    }

    func exitFullScreen() {
        if let window, window.styleMask.contains(.fullScreen) { window.toggleFullScreen(nil) }
    }

    func disconnect() {
        // Closing the window ends the session (ViewerView.onDisappear).
        window?.close()
    }

    func hide() {
        UserDefaults.standard.set(false, forKey: Self.visibleKey)
        session?.showToast("Session control hidden · ⌃⌥⌘ still releases; Control → Show Session Control brings it back")
    }

    // MARK: Displays

    func showMainDisplay() {
        if let session { CoreModel.shared.showDisplay(RemoteDisplay(), for: session.id) }
    }

    /// A display that fills a screen of this Mac, placed as the menu's arrangement says.
    func showVirtualDisplay(_ screen: ScreenFit) {
        guard let session, case .connected(let info) = session.phase else { return }
        showDisplay(screen.display(preferredArrangement(info)))
    }

    /// Asks for `display` (the host's own screen, or a virtual one: a size picked, a retry). A
    /// size that fills a screen attached here is remembered as fitted to it, and so is one that
    /// was fitted to a screen before (an offer accepted while that screen is away).
    func showDisplay(_ display: RemoteDisplay) {
        guard let session else { return }
        guard display.kind == .virtual else { return showMainDisplay() }
        let sameSize = { (size: CGSize) in size == CGSize(width: Int(display.width), height: Int(display.height)) }
        let matched = ScreenFit.all().first { sameSize($0.size) }?.size
            ?? RememberedDisplay.load(session.hostId)?.matchedScreen.flatMap { sameSize($0) ? $0 : nil }
        CoreModel.shared.showDisplay(display, for: session.id, matchedScreen: matched)
    }

    /// The display shown, or the one the session is switching to: a pick made meanwhile builds on
    /// that, so it keeps what was just picked.
    private func current(_ session: SessionModel, _ info: SessionInfo) -> RemoteDisplay {
        session.display.isSwitching ? (session.requestedDisplay?.display ?? info.display) : info.display
    }

    /// The same display, placed differently among the host's own. On the host's own screen, where
    /// the next display picked goes.
    func arrange(_ arrangement: RemoteDisplay.Arrangement) {
        guard let session, case .connected(let info) = session.phase else { return }
        var display = current(session, info)
        guard display.kind == .virtual else {
            nextArrangement = arrangement
            return
        }
        display.arrangement = arrangement
        showDisplay(display)
    }

    /// Opens the Custom Size sheet with the display shown, or this screen's size.
    func editCustomDisplay() {
        guard let session, case .connected(let info) = session.phase else { return }
        let shown = current(session, info)
        if shown.kind == .virtual {
            customDisplay = DisplayDraft(width: shown.width, height: shown.height, hidpi: shown.hidpi, refreshHz: shown.refreshHz)
        } else if let screen = thisScreen ?? ScreenFit.main() {
            customDisplay = DisplayDraft(width: screen.width, height: screen.height, hidpi: screen.hidpi, refreshHz: screen.refreshHz)
        } else {
            customDisplay = DisplayDraft(width: 2560, height: 1440, hidpi: false, refreshHz: 60)
        }
    }

    /// From the Custom Size sheet.
    func applyCustomDisplay(_ display: RemoteDisplay) {
        customDisplay = nil
        guard let session, case .connected(let info) = session.phase else { return }
        var display = display
        display.arrangement = preferredArrangement(info)
        showDisplay(display)
    }

    /// Where a display picked now goes: where the one shown is, or the one last used with this
    /// host; only next to the host's own screens on the same Mac.
    func preferredArrangement(_ info: SessionInfo) -> RemoteDisplay.Arrangement {
        if info.displayAvailable == DisplayReason.sameMac || info.sameMachine { return .extend }
        let shown = session.map { current($0, info) } ?? info.display
        if shown.kind == .virtual { return shown.arrangement }
        return nextArrangement ?? RememberedDisplay.load(info.hostId)?.display.arrangement ?? .only
    }

    /// Whether the session shows a display that fills this window's screen: in full screen it's
    /// pixel for pixel, so the toolbar stays out of the way.
    func fillsThisScreen(_ display: RemoteDisplay) -> Bool {
        guard display.kind == .virtual, let thisScreen else { return false }
        return display.width == thisScreen.width && display.height == thisScreen.height
    }

    // MARK: Placement

    private var placementKey: String? {
        guard let session else { return nil }
        return "sessionControl.\(session.hostId.isEmpty ? session.target : session.hostId)"
    }

    /// Puts the control where the user left it on this Mac last time.
    func restorePlacement() {
        guard let key = placementKey, let saved = UserDefaults.standard.dictionary(forKey: key),
              let restored = Placement(saved), restored != placement else { return }
        placement = restored
    }

    func dock(at new: Placement) {
        placement = new
        if let key = placementKey { UserDefaults.standard.set(new.dictionary, forKey: key) }
    }

    /// Where the control is or may expand to, for other overlays to keep clear of (so they don't
    /// move whenever it expands). Remote-screen points, top-left origin.
    var reservedFrame: CGRect {
        guard screenSize.width > 0, screenSize.height > 0 else { return .null }
        let tab = Docking.frame(of: Docking.tabSize(placement.edge), at: placement, in: screenSize)
        return tab.union(Docking.frame(of: expandedSize, at: placement, in: screenSize))
    }

    // MARK: From the views

    /// MetalHostView: the forwarder to act through, and the window (full screen, closing).
    func attach(forwarder: InputForwarder, window: NSWindow?) {
        self.forwarder = forwarder
        guard let window, window !== self.window else { return }
        windowObservers.forEach(NotificationCenter.default.removeObserver)
        self.window = window
        windowObservers = [NSWindow.didEnterFullScreenNotification, NSWindow.didExitFullScreenNotification].map { name in
            NotificationCenter.default.addObserver(forName: name, object: window, queue: .main) { [weak self] _ in
                MainActor.assumeIsolated { self?.syncFullScreen() }
            }
        }
        // The window moved to another screen, or screens came, went or changed resolution: the
        // Display menu's sizes follow.
        for (name, object) in [(NSWindow.didChangeScreenNotification, window as AnyObject?),
                               (NSApplication.didChangeScreenParametersNotification, nil)] {
            let observer = NotificationCenter.default.addObserver(forName: name, object: object, queue: .main) { [weak self] _ in
                MainActor.assumeIsolated { self?.syncScreens() }
            }
            windowObservers.append(observer)
        }
        // Called while SwiftUI updates the view: publish afterwards.
        DispatchQueue.main.async { [weak self] in
            MainActor.assumeIsolated {
                self?.syncFullScreen()
                self?.syncScreens()
            }
        }
    }

    private func syncFullScreen() {
        let fullScreen = window?.styleMask.contains(.fullScreen) == true
        if fullScreen != isFullScreen { isFullScreen = fullScreen }
    }

    private func syncScreens() {
        let all = ScreenFit.all()
        let screen = window?.screen.map(ScreenFit.init)
        let others = all.filter { $0.id != screen?.id }
        if screen != thisScreen { thisScreen = screen }
        if others != otherScreens { otherScreens = others }
    }

    /// From InputForwarder whenever forwarding starts or stops or the user releases or resumes.
    /// Published a moment later, because it may come from inside a SwiftUI update
    /// (RemoteScreen.updateNSView), and only the latest state counts.
    func inputChanged(forwarding: Bool, released: Bool) {
        let queued = pendingInput != nil
        pendingInput = (forwarding, released)
        guard !queued else { return }
        DispatchQueue.main.async { [weak self] in
            MainActor.assumeIsolated { self?.applyPendingInput() }
        }
    }

    private func applyPendingInput() {
        guard let (forwarding, released) = pendingInput else { return }
        pendingInput = nil
        if forwarding && !isForwarding { forwardingStarted() }
        // Show the released state (and how to come back) for a moment.
        if released && !isReleased { flash() }
        if forwarding != isForwarding { isForwarding = forwarding }
        if released != isReleased { isReleased = released }
    }

    /// Says how to get out: opens briefly each time control starts in full screen, where nothing
    /// else says it, and the first few times in a window.
    private func forwardingStarted() {
        let controlStarted = !forwardedSinceControlStarted
        forwardedSinceControlStarted = true
        let defaults = UserDefaults.standard
        guard defaults.object(forKey: Self.visibleKey) as? Bool ?? true else { return }
        if window?.styleMask.contains(.fullScreen) == true {
            flash()
            return
        }
        guard controlStarted else { return }
        let shown = defaults.integer(forKey: Self.windowedFlashesKey)
        guard shown < Self.windowedFlashLimit else { return }
        defaults.set(shown + 1, forKey: Self.windowedFlashesKey)
        flash()
    }

    private func flash(seconds: Double = 2) {
        flashGeneration += 1
        let generation = flashGeneration
        isFlashing = true
        DispatchQueue.main.asyncAfter(deadline: .now() + seconds) { [weak self] in
            MainActor.assumeIsolated {
                guard let self, self.flashGeneration == generation else { return }
                self.isFlashing = false
            }
        }
    }

    /// The control's frame as laid out (null when it's hidden).
    func controlMoved(to frame: CGRect) {
        self.frame = frame
        if frame.isNull, isPointerNear { isPointerNear = false }
    }

    func screenResized(to size: CGSize) {
        if size != screenSize { screenSize = size }
    }

    func expandedResized(to size: CGSize) {
        if size != expandedSize, size.width > 0 { expandedSize = size }
    }

    /// Whether a point on the remote screen (points, top-left origin) is over the control, where
    /// the mouse is this Mac's: moves, clicks and scrolls there aren't sent.
    func excludes(_ point: CGPoint) -> Bool {
        !frame.isNull && frame.insetBy(dx: -Self.exclusionMargin, dy: -Self.exclusionMargin).contains(point)
    }

    /// From MetalHostView's mouse moves (nil: the pointer left the screen). Published only when
    /// it crosses the distance (going away, a while later).
    func pointerMoved(to point: CGPoint?) {
        let near = point.map { !frame.isNull && frame.insetBy(dx: -Self.peekDistance, dy: -Self.peekDistance).contains($0) } ?? false
        if near {
            peekGeneration += 1
            if !isPointerNear { isPointerNear = true }
            return
        }
        // One pending fade at a time: further moves away don't queue more.
        guard isPointerNear, fadePendingFor != peekGeneration else { return }
        let generation = peekGeneration
        fadePendingFor = generation
        DispatchQueue.main.asyncAfter(deadline: .now() + Self.peekLinger) { [weak self] in
            MainActor.assumeIsolated {
                guard let self, self.peekGeneration == generation else { return }
                self.isPointerNear = false
            }
        }
    }

    /// For `--snapshot`: a state to show, without a window or input behind it.
    func showSample(forwarding: Bool = false, released: Bool = false, fullScreen: Bool = false,
                    expanded: Bool = false, placement: Placement = .default) {
        isForwarding = forwarding
        isReleased = released
        isFullScreen = fullScreen
        isFlashing = expanded
        self.placement = placement
    }
}

/// Where the session control is docked: an edge of the remote screen and a position along it.
struct Placement: Equatable {
    enum Edge: String { case top, bottom, left, right }

    var edge: Edge
    /// 0...1 along the edge (left to right, top to bottom), between the corner keep-outs.
    var fraction: CGFloat

    /// Top centre: the emptiest part of a Mac's menu bar (menus on the left, status items on the
    /// right).
    static let `default` = Placement(edge: .top, fraction: 0.5)

    init(edge: Edge, fraction: CGFloat) {
        self.edge = edge
        self.fraction = fraction.isFinite ? min(max(fraction, 0), 1) : 0.5
    }

    init?(_ dictionary: [String: Any]) {
        guard let raw = dictionary["edge"] as? String, let edge = Edge(rawValue: raw),
              let fraction = dictionary["fraction"] as? Double else { return nil }
        self.init(edge: edge, fraction: fraction)
    }

    var dictionary: [String: Any] { ["edge": edge.rawValue, "fraction": Double(fraction)] }

    var isHorizontal: Bool { edge == .top || edge == .bottom }
}

/// Geometry of the docked control, in remote-screen points (top-left origin).
enum Docking {
    /// Gap to the docked edge: the outermost strip stays the remote Mac's, so its menu bar and
    /// Dock still reveal there.
    static let edgeInset: CGFloat = 6
    /// Kept clear of each corner: the remote Mac's hot corners, the window's rounded corners.
    static let cornerKeepOut: CGFloat = 48
    /// Dropped this close to the middle of an edge, it snaps to the middle.
    static let centreDetent: CGFloat = 16
    /// The collapsed tab, lying along its edge.
    static let tabLength: CGFloat = 60
    static let tabThickness: CGFloat = 22

    static func tabSize(_ edge: Placement.Edge) -> CGSize {
        edge == .top || edge == .bottom
            ? CGSize(width: tabLength, height: tabThickness)
            : CGSize(width: tabThickness, height: tabLength)
    }

    /// Where a control of this size sits: centred on its anchor along the edge, against the
    /// edge, and inside the screen when it's wider than the room between the keep-outs.
    static func frame(of size: CGSize, at placement: Placement, in screen: CGSize) -> CGRect {
        let anchor = anchor(placement.fraction, along: placement.isHorizontal ? screen.width : screen.height)
        let x: CGFloat, y: CGFloat
        switch placement.edge {
        case .top: (x, y) = (anchor - size.width / 2, edgeInset)
        case .bottom: (x, y) = (anchor - size.width / 2, screen.height - edgeInset - size.height)
        case .left: (x, y) = (edgeInset, anchor - size.height / 2)
        case .right: (x, y) = (screen.width - edgeInset - size.width, anchor - size.height / 2)
        }
        return CGRect(x: clamp(x, edgeInset, screen.width - edgeInset - size.width),
                      y: clamp(y, edgeInset, screen.height - edgeInset - size.height),
                      width: size.width, height: size.height)
    }

    /// The tab's centre along an edge of this length: that fraction of the way between the
    /// keep-outs (the middle when there's no room).
    static func anchor(_ fraction: CGFloat, along length: CGFloat) -> CGFloat {
        let (low, high) = track(length)
        return high > low ? low + fraction * (high - low) : length / 2
    }

    /// The placement nearest to a control of this size dropped with its centre here: the edge its
    /// own side comes closest to, at that position along it. (From its centre, a pill wider than
    /// the screen is tall could never reach the left or right edge.)
    static func snap(_ centre: CGPoint, size: CGSize, in screen: CGSize) -> Placement {
        let (w, h) = (size.width / 2, size.height / 2)
        let distances: [(Placement.Edge, CGFloat)] = [
            (.top, centre.y - h), (.bottom, screen.height - centre.y - h),
            (.left, centre.x - w), (.right, screen.width - centre.x - w),
        ]
        let edge = distances.reduce(distances[0]) { $1.1 < $0.1 ? $1 : $0 }.0
        let horizontal = edge == .top || edge == .bottom
        let along = horizontal ? centre.x : centre.y
        let length = horizontal ? screen.width : screen.height
        let (low, high) = track(length)
        guard high > low, abs(along - length / 2) > centreDetent else { return Placement(edge: edge, fraction: 0.5) }
        return Placement(edge: edge, fraction: (along - low) / (high - low))
    }

    /// The range the tab's centre may take along an edge.
    private static func track(_ length: CGFloat) -> (CGFloat, CGFloat) {
        (cornerKeepOut + tabLength / 2, length - cornerKeepOut - tabLength / 2)
    }

    private static func clamp(_ value: CGFloat, _ low: CGFloat, _ high: CGFloat) -> CGFloat {
        min(max(value, low), max(low, high))
    }
}

/// The session control: a small tab docked to an edge of the remote screen that expands, on hover
/// or click, into a pill with the input state, the main action (Release, Resume, Control), a menu
/// and a grip to move it. Fills the remote screen; only the control itself takes clicks.
struct SessionControl: View {
    @ObservedObject var session: SessionModel
    @ObservedObject var model: SessionControlModel

    @AppStorage(SessionControlModel.visibleKey) private var visible = true
    @AppStorage(SessionControlModel.pinnedKey) private var pinned = false
    @AppStorage(SystemShortcuts.defaultsKey) private var sendSystemShortcuts = true
    @AppStorage(TrackpadGestures.defaultsKey) private var sendTrackpadGestures = true
    @AppStorage(SharedClipboard.defaultsKey) private var shareClipboard = true
    @AppStorage("showStats") private var showStats = true
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.accessibilityReduceTransparency) private var reduceTransparency
    @Environment(\.accessibilityVoiceOverEnabled) private var voiceOver
    @Environment(\.colorSchemeContrast) private var contrast

    @State private var hovering = false
    @State private var hoverExpanded = false
    @State private var hoverTask: Task<Void, Never>?
    @State private var menuOpen = false
    @State private var drag: Drag?
    /// Reduce Motion: after a drop, fades in where it landed instead of travelling there.
    @State private var landing = false

    static let pillHeight: CGFloat = 36
    /// The named coordinate space of the remote screen, for frames and drags.
    private static let space = "remoteScreen"
    /// Below this, a press on the tab or grip is a click.
    private static let dragThreshold: CGFloat = 4
    /// Hovering this long expands it (a pointer passing on its way to the menu bar doesn't).
    private static let hoverDwell: Duration = .milliseconds(200)
    /// Away this long, it collapses.
    private static let leaveDelay: Duration = .milliseconds(800)

    private struct Drag {
        /// Pointer minus the control's centre when the drag started.
        var grab: CGSize
        var centre: CGPoint
        var expanded: Bool
    }

    var body: some View {
        DockLayout(placement: model.placement, dragCentre: drag?.centre, ghost: ghostPlacement) {
            if model.isShown(enabled: visible) {
                control
                if let target = ghostPlacement {
                    ghost(at: target).layoutValue(key: IsGhost.self, value: true)
                }
            }
        }
        .coordinateSpace(name: Self.space)
        .onGeometryChange(for: CGSize.self) { $0.size } action: { model.screenResized(to: $0) }
        .onAppear { model.restorePlacement() }
        .animation(reduceMotion ? .easeOut(duration: 0.1) : .easeOut(duration: 0.15), value: expanded)
        // Its menu is open (or another, while the pointer is on it): stay expanded until it closes.
        .onReceive(NotificationCenter.default.publisher(for: NSMenu.didBeginTrackingNotification)) { _ in
            guard hovering else { return }
            menuOpen = true
            hoverExpanded = true
        }
        .onReceive(NotificationCenter.default.publisher(for: NSMenu.didEndTrackingNotification)) { _ in
            guard menuOpen else { return }
            menuOpen = false
            if !hovering { scheduleCollapse() }
        }
    }

    private var state: SessionControlModel.State { model.state }

    private var expanded: Bool {
        // Unchanged while dragged: swapping the tab and the pill would cancel the drag.
        if let drag { return drag.expanded }
        return pinned || hoverExpanded || model.isFlashing
    }

    private var hostName: String { "“\(session.hostName)”" }

    // MARK: Control

    private var control: some View {
        ZStack(alignment: edgeAlignment) {
            if expanded {
                pill.transition(reduceMotion ? .opacity : .opacity.combined(with: .scale(scale: 0.94, anchor: edgeAnchor)))
            } else {
                tab.transition(.opacity)
            }
        }
        .environment(\.colorScheme, .dark)
        .opacity(landing ? 0 : idleOpacity)
        .animation(.easeOut(duration: 0.2), value: idleOpacity)
        .onHover(perform: hover)
        .onGeometryChange(for: CGRect.self) { $0.frame(in: .named(Self.space)) } action: { model.controlMoved(to: $0) }
        .onDisappear {
            // Hidden (state, setting) even mid-drag: that drag never ends.
            model.controlMoved(to: .null)
            drag = nil
        }
        .accessibilityElement(children: .contain)
        .accessibilityLabel("Session controls for \(hostName)")
    }

    /// Faded while it's out of the way and input goes to the remote Mac (or it's only watched),
    /// never when that could hide it from someone who needs it.
    private var idleOpacity: Double {
        let idle = !expanded && !hovering && !model.isPointerNear && drag == nil
        let quiet = state == .forwarding || state == .viewing || state == .refused || state == .requesting
        let alwaysClear = contrast == .increased || reduceTransparency || voiceOver || NSApp.isFullKeyboardAccessEnabled
        return idle && quiet && !alwaysClear ? 0.65 : 1
    }

    /// Collapsed: the state at a glance, and a chevron pointing into the screen.
    private var tab: some View {
        let vertical = !model.placement.isHorizontal
        let stack = vertical ? AnyLayout(VStackLayout(spacing: 4)) : AnyLayout(HStackLayout(spacing: 5))
        let size = Docking.tabSize(model.placement.edge)
        return stack {
            tabGlyph
            Image(systemName: chevron)
                .font(.system(size: 8, weight: .bold))
                .foregroundStyle(.white.opacity(0.6))
        }
        .frame(width: size.width, height: size.height)
        .background { hud(Capsule()) }
        .contentShape(Capsule())
        .gesture(dragGesture { expandNow() })
        .help("Session controls for \(hostName)")
        // Keyboard navigation only: a click mustn't take the focus.
        .focusable(interactions: .activate)
        .focusEffectDisabled(!NSApp.isFullKeyboardAccessEnabled)
        .onKeyPress(.space) { expandNow(); return .handled }
        .onKeyPress(.return) { expandNow(); return .handled }
        .accessibilityElement()
        .accessibilityLabel("Show session controls")
        .accessibilityValue(stateDescription)
        .accessibilityAddTraits(.isButton)
        .accessibilityAction { expandNow() }
    }

    /// The same sign as at the start of the expanded label (a spinner while asking).
    @ViewBuilder private var tabGlyph: some View {
        if state == .requesting {
            ProgressView().controlSize(.mini).scaleEffect(0.8).frame(width: 12, height: 12)
        } else {
            stateIcon.font(.system(size: 11, weight: glyphWeight))
        }
    }

    /// Expanded: what's happening, the one thing to do about it, everything else, and the grip.
    private var pill: some View {
        HStack(spacing: 8) {
            HStack(spacing: 7) {
                stateIcon.font(.system(size: 11, weight: glyphWeight))
                if !compact {
                    Text(stateDescription)
                        .font(.system(size: 12, weight: .medium))
                        .foregroundStyle(.white)
                        .lineLimit(1)
                        .fixedSize()
                }
            }
            .help(compact ? stateDescription : "")
            .accessibilityElement(children: .combine)
            primaryButton
            Rectangle().fill(.white.opacity(0.14)).frame(width: 1, height: 16)
            moreMenu
            grip
        }
        .padding(.leading, 13)
        .padding(.trailing, 4)
        .frame(height: Self.pillHeight)
        .background { hud(Capsule()) }
        .contentShape(Capsule())
        .onGeometryChange(for: CGSize.self) { $0.size } action: { model.expandedResized(to: $0) }
        .onExitCommand { collapse() }
    }

    /// Narrow windows: icons only (their tooltips say the rest).
    private var compact: Bool { model.screenSize.width > 0 && model.screenSize.width < 520 }

    @ViewBuilder private var stateIcon: some View {
        switch state {
        case .forwarding:
            Circle().fill(Color.lkAccent).frame(width: 7, height: 7)
        case .viewing, .requesting:
            Image(systemName: "eye").foregroundStyle(.white.opacity(0.85))
        case .released:
            Image(systemName: "pause.fill").foregroundStyle(.white.opacity(0.85))
        case .paused:
            Image(systemName: "pause").foregroundStyle(.white.opacity(0.85))
        case .refused:
            Image(systemName: "hand.raised").foregroundStyle(Color(hex: 0xE5B45A))
        }
    }

    private var stateDescription: String {
        switch state {
        case .forwarding: "Controlling \(hostName)"
        case .released: "Released · click the screen to control"
        case .paused: "Paused · click the screen to control"
        case .viewing, .refused, .requesting: "Viewing \(hostName)"
        }
    }

    @ViewBuilder private var primaryButton: some View {
        switch state {
        case .forwarding:
            Button(action: model.release) {
                HStack(spacing: 6) {
                    Text("Release")
                    Text("⌃⌥⌘").font(.system(size: 11, weight: .medium)).foregroundStyle(.white.opacity(0.6))
                }
            }
            .buttonStyle(HUDButtonStyle())
            .help("Use your keyboard and pointer on this Mac again (⌃⌥⌘)")
            .accessibilityLabel("Release keyboard and mouse")
            .accessibilityHint("Your keyboard and pointer go back to this Mac. Shortcut: Control-Option-Command.")
        case .released, .paused:
            Button(action: model.resume) {
                Label("Resume", systemImage: "play.fill").labelStyle(HUDLabelStyle())
            }
            .buttonStyle(HUDButtonStyle(prominent: true))
            .help("Control \(hostName) again (⌃⌥⌘, or click the screen)")
            .accessibilityLabel("Resume controlling \(hostName)")
        case .viewing, .refused:
            Button("Control", action: model.requestControl)
                .buttonStyle(HUDButtonStyle(prominent: true))
                .help("Control \(hostName) with your mouse and keyboard (⌃⌥⌘)")
                .accessibilityLabel("Control \(hostName)")
        case .requesting:
            Button {} label: {
                HStack(spacing: 6) {
                    ProgressView().controlSize(.mini).scaleEffect(0.8).frame(width: 12, height: 12)
                    Text("Asking…")
                }
            }
            .buttonStyle(HUDButtonStyle())
            .disabled(true)
            .accessibilityLabel("Asking \(hostName) for control")
        }
    }

    private var moreMenu: some View {
        Menu {
            menuItems
        } label: {
            Image(systemName: "ellipsis")
                .font(.system(size: 13, weight: .semibold))
                .foregroundStyle(.white.opacity(0.85))
                .frame(width: 28, height: 28)
                .contentShape(Circle())
        }
        .menuStyle(.button)
        .buttonStyle(.plain)
        .menuIndicator(.hidden)
        .fixedSize()
        .help("More")
        .accessibilityLabel("More session actions")
    }

    @ViewBuilder private var menuItems: some View {
        Section("On \(hostName)") {
            ForEach(RemoteAction.allCases) { action in
                Button(action.title) { model.perform(action) }
            }
        }
        .disabled(!session.isControlling)
        Divider()
        DisplayMenu(session: session, controls: model)
        Divider()
        Toggle("Send System Shortcuts to Remote Mac", isOn: $sendSystemShortcuts)
        Toggle("Send Trackpad Gestures to Remote Mac", isOn: $sendTrackpadGestures)
        Toggle("Share Clipboard", isOn: $shareClipboard)
        Toggle("Share Microphone", isOn: Binding(get: { session.microphone != .off },
                                                 set: { CoreModel.shared.setMicrophone($0, for: session.id) }))
        Divider()
        if session.mode == .control {
            Button("View Only", action: model.viewOnly)
        } else {
            Button("Control", action: model.requestControl)
        }
        Toggle("Show Statistics", isOn: $showStats)
        if model.isFullScreen {
            Button("Exit Full Screen", action: model.exitFullScreen)
        }
        Toggle("Keep Expanded", isOn: $pinned)
        Button("Hide Session Control", action: model.hide)
        Divider()
        Button("Disconnect", role: .destructive, action: model.disconnect)
    }

    /// Drag it to another edge; double-click to put it back at the top.
    private var grip: some View {
        Grid(horizontalSpacing: 3, verticalSpacing: 3) {
            ForEach(0..<3, id: \.self) { _ in
                GridRow {
                    Circle().frame(width: 2.5, height: 2.5)
                    Circle().frame(width: 2.5, height: 2.5)
                }
            }
        }
        .foregroundStyle(.white.opacity(0.45))
        .frame(width: 20, height: 28)
        .contentShape(Rectangle())
        .onHover { inside in
            guard drag == nil else { return }
            (inside ? NSCursor.openHand : NSCursor.arrow).set()
        }
        .gesture(dragGesture {
            if (NSApp.currentEvent?.clickCount ?? 1) >= 2 { land(at: .default) }
        })
        .help("Drag to another edge · double-click to put it back at the top")
        .accessibilityHidden(true)
    }

    private var ghostPlacement: Placement? {
        guard let drag, model.screenSize.width > 0 else { return nil }
        return Docking.snap(drag.centre, size: model.frame.size, in: model.screenSize)
    }

    /// Where it will snap to when dropped.
    private func ghost(at target: Placement) -> some View {
        let size = drag?.expanded == true ? model.frame.size : Docking.tabSize(target.edge)
        return Capsule()
            .strokeBorder(.white.opacity(0.7), style: StrokeStyle(lineWidth: 1.5, dash: [4, 3]))
            .background(Capsule().fill(.black.opacity(0.2)))
            .frame(width: size.width, height: size.height)
            .allowsHitTesting(false)
            .accessibilityHidden(true)
    }

    // MARK: Look

    private var highContrast: Bool { contrast == .increased }

    private var glyphWeight: Font.Weight { highContrast ? .bold : .semibold }

    /// The forced-dark HUD look of ControlBanner, legible on any remote screen: a dark fill, a
    /// light inner stroke for dark content and a dark hairline and shadow for light content.
    private func hud<S: InsettableShape>(_ shape: S) -> some View {
        shape
            .fill(.black.opacity(highContrast ? 0.95 : reduceTransparency ? 0.92 : 0.72))
            .overlay(shape.strokeBorder(.white.opacity(highContrast ? 0.6 : 0.12), lineWidth: highContrast ? 1.5 : 1))
            .overlay(shape.inset(by: -0.5).stroke(.black.opacity(0.35), lineWidth: 0.5))
            .shadow(color: .black.opacity(0.35), radius: 8, y: 2)
    }

    private var edgeAlignment: Alignment {
        switch model.placement.edge {
        case .top: .top
        case .bottom: .bottom
        case .left: .leading
        case .right: .trailing
        }
    }

    private var edgeAnchor: UnitPoint {
        switch model.placement.edge {
        case .top: .top
        case .bottom: .bottom
        case .left: .leading
        case .right: .trailing
        }
    }

    private var chevron: String {
        switch model.placement.edge {
        case .top: "chevron.down"
        case .bottom: "chevron.up"
        case .left: "chevron.right"
        case .right: "chevron.left"
        }
    }

    // MARK: Expanding and collapsing

    private func hover(_ inside: Bool) {
        // A remote drag passing over it (a button held since a press on the screen): stay inert.
        if inside && NSEvent.pressedMouseButtons != 0 && drag == nil { return }
        hovering = inside
        hoverTask?.cancel()
        if inside {
            guard !hoverExpanded else { return }
            hoverTask = Task { @MainActor in
                try? await Task.sleep(for: Self.hoverDwell)
                guard !Task.isCancelled, hovering, NSEvent.pressedMouseButtons == 0 else { return }
                hoverExpanded = true
            }
        } else {
            scheduleCollapse()
        }
    }

    private func scheduleCollapse() {
        hoverTask?.cancel()
        hoverTask = Task { @MainActor in
            try? await Task.sleep(for: Self.leaveDelay)
            guard !Task.isCancelled, !hovering, !menuOpen, drag == nil else { return }
            hoverExpanded = false
        }
    }

    /// A click, Space or Return on the tab, or VoiceOver's action. Stays open until the pointer
    /// leaves it (or Esc), so keyboard and VoiceOver users have time to reach the buttons.
    private func expandNow() {
        hoverTask?.cancel()
        hoverExpanded = true
    }

    private func collapse() {
        hoverTask?.cancel()
        hoverExpanded = false
    }

    // MARK: Moving

    /// Presses on the tab or the grip: a click below the threshold, a drag beyond it. The control
    /// follows the pointer, and snaps to the nearest edge when dropped.
    private func dragGesture(click: @escaping () -> Void) -> some Gesture {
        DragGesture(minimumDistance: 0, coordinateSpace: .named(Self.space))
            .onChanged { value in
                if drag == nil {
                    guard hypot(value.translation.width, value.translation.height) >= Self.dragThreshold,
                          !model.frame.isNull else { return }
                    let frame = model.frame
                    drag = Drag(grab: CGSize(width: value.startLocation.x - frame.midX, height: value.startLocation.y - frame.midY),
                                centre: CGPoint(x: frame.midX, y: frame.midY), expanded: expanded)
                }
                guard var current = drag else { return }
                let half = CGSize(width: model.frame.width / 2, height: model.frame.height / 2)
                let screen = model.screenSize
                current.centre = CGPoint(
                    x: min(max(value.location.x - current.grab.width, half.width), max(half.width, screen.width - half.width)),
                    y: min(max(value.location.y - current.grab.height, half.height), max(half.height, screen.height - half.height)))
                drag = current
                if NSCursor.current !== NSCursor.closedHand { NSCursor.closedHand.set() }
            }
            .onEnded { _ in
                guard let finished = drag else { return click() }
                land(at: Docking.snap(finished.centre, size: model.frame.size, in: model.screenSize))
                NSCursor.arrow.set()
            }
    }

    /// Docks it, travelling there (or, with Reduce Motion, fading in there).
    private func land(at target: Placement) {
        guard reduceMotion else {
            withAnimation(.easeOut(duration: 0.18)) {
                drag = nil
                model.dock(at: target)
            }
            return
        }
        var instant = Transaction()
        instant.disablesAnimations = true
        withTransaction(instant) {
            drag = nil
            landing = true
            model.dock(at: target)
        }
        DispatchQueue.main.async {
            withAnimation(.easeOut(duration: 0.1)) { landing = false }
        }
    }
}

/// Places the control against its edge, or under the pointer while it's dragged, and the drop
/// target's outline where it will snap to. Takes the whole screen, but only its children take
/// clicks.
private struct DockLayout: Layout {
    var placement: Placement
    var dragCentre: CGPoint?
    var ghost: Placement?

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) -> CGSize {
        proposal.replacingUnspecifiedDimensions()
    }

    func placeSubviews(in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) {
        for subview in subviews {
            let size = subview.sizeThatFits(.unspecified)
            let centre: CGPoint
            if subview[IsGhost.self] {
                let frame = Docking.frame(of: size, at: ghost ?? placement, in: bounds.size)
                centre = CGPoint(x: frame.midX, y: frame.midY)
            } else if let dragCentre {
                centre = dragCentre
            } else {
                let frame = Docking.frame(of: size, at: placement, in: bounds.size)
                centre = CGPoint(x: frame.midX, y: frame.midY)
            }
            subview.place(at: CGPoint(x: bounds.minX + centre.x, y: bounds.minY + centre.y), anchor: .center,
                          proposal: ProposedViewSize(size))
        }
    }
}

private struct IsGhost: LayoutValueKey {
    static let defaultValue = false
}

/// A button on the dark session control: white text on a light wash, or on the accent colour for
/// the inviting action (Resume, Control).
private struct HUDButtonStyle: ButtonStyle {
    var prominent = false
    @Environment(\.isEnabled) private var isEnabled
    @Environment(\.colorSchemeContrast) private var contrast

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(.system(size: 12, weight: .semibold))
            .foregroundStyle(.white.opacity(isEnabled ? 1 : 0.6))
            .padding(.horizontal, 11)
            .frame(height: 28)
            .background(Capsule().fill(fill(pressed: configuration.isPressed)))
            .overlay(Capsule().strokeBorder(.white.opacity(contrast == .increased ? 0.6 : 0)))
            .contentShape(Capsule())
    }

    private func fill(pressed: Bool) -> Color {
        if prominent { return Color.lkAccent.opacity(pressed ? 0.75 : 1) }
        return .white.opacity(pressed ? 0.24 : isEnabled ? 0.14 : 0.08)
    }
}

/// A small icon before the title, for HUD buttons.
private struct HUDLabelStyle: LabelStyle {
    func makeBody(configuration: Configuration) -> some View {
        HStack(spacing: 5) {
            configuration.icon.font(.system(size: 9, weight: .bold))
            configuration.title
        }
    }
}
