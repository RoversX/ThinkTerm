import Foundation
import UIKit
import QuartzCore
import Combine
import SwiftUI
import CoreText
import CoreGraphics

/// The shell's side of the thread contract. `Notify` calls arrive on the
/// core thread; nothing here blocks them, and nothing here calls back into
/// the core from inside them. The display link is the only place `render`
/// is called; the core only ever *asks* for a frame.
///
/// One model per connection: the screen that shows a host makes one and
/// shuts it down when it goes.
final class TerminalModel: ObservableObject, @unchecked Sendable {
    // The App's views, refreshed when the core says something changed.
    @Published var tabs: TabsView?
    @Published var threads: ThreadsView?
    /// Whole tabs, as they are split, by the pane a card shows, for the
    /// overview's cards, and their pictures, drawn off the main thread
    /// (`paintScreen`): asked for while it is up, let go when it closes
    /// (`releaseScreens`).
    @Published private(set) var screens: [Int: TabPreview] = [:]
    @Published private(set) var pictures: [Int: CGImage] = [:]
    /// Moves with each screen that comes in: its picture is drawn again.
    @Published private(set) var screenSerials: [Int: Int] = [:]
    private var screensWanted = false
    /// When each pane's screen was asked for, until it comes: one question
    /// a pane at a time, asked again only if it goes long unanswered.
    private var screenAsks: [Int: ContinuousClock.Instant] = [:]
    /// Moves when the screens are let go: a picture drawn before is not kept.
    private var paintTurn = 0
    private let painter = DispatchQueue(label: "thinkterm.screen-pictures", qos: .userInitiated)
    @Published var sidebar: SidebarView?
    /// Moves when the tab icon cards change (`Core.tabIconsRevision`): the
    /// cards on show are drawn again.
    @Published private(set) var iconsRevision: UInt64 = 0
    /// A menu action that ends something (`delete-thread:…`), awaiting a yes.
    @Published var confirming: String?
    /// Counts the sidebar's "+": the screen asks for the folder.
    @Published private(set) var nameAsks = 0
    @Published var tree: TreeView?
    @Published var navs: [NavView] = []
    /// The panes of the tab on show, for the switcher under the terminal.
    @Published var panes: PanesView?
    /// Every pane an agent runs in, for the right sidebar.
    @Published var agents: AgentsView?
    /// Files on their way to the host, and those that just got there or
    /// did not (`upload`): shown over the keys until they have said so.
    @Published private(set) var uploads: [Upload] = []
    private var uploadTimer: Timer?
    /// An upload ended, one way or the other (the self-test listens).
    var onUploadEnded: ((Upload) -> Void)?

    struct Upload: Identifiable, Equatable {
        let id: UInt64
        let name: String
        /// The app's own copy, read as it is sent and removed after.
        let file: URL
        var sent: UInt64 = 0
        var total: UInt64 = 0
        /// Where it is on the host, once it is.
        var path: String?
        var error: String?
        var ended: Date?
    }
    @Published var status: StatusView?
    @Published var title = ""
    @Published var connection = ""
    /// The key panel is up in the keyboard's place.
    #if DEBUG
    @Published var panelOpen = ProcessInfo.processInfo.arguments.contains("--keypanel")
    #else
    @Published var panelOpen = false
    #endif
    /// A thread this phone was on last time it was in this host, put
    /// back once the threads are listed after a connect.
    private var restoredThread = false
    /// The connection whose views were out when this one was asked for:
    /// its listing is not this connection's first.
    private var connectionBefore: UInt64 = 0
    private var lastThreadKey: String? { host.map { "thread.last." + $0.id.uuidString } }

    var isConnected: Bool {
        !connection.isEmpty && !connection.hasPrefix("disconnected") && !connection.hasPrefix("failed") && !connection.hasPrefix("idle")
    }
    @Published var attached = false
    @Published var composing: String?
    @Published var ctrlSticky = false
    @Published var altSticky = false
    @Published var logText = ""
    @Published var stats = ""
    @Published var inset: CGFloat = 0
    /// A selection was just copied; the status line says so for a moment.
    @Published var copied = false
    /// The App's remark for the status line: a passing one goes after a
    /// few seconds, a sticky one (the connection is down) stays until the
    /// App takes it back.
    @Published private(set) var toastText: String?
    /// `toastText` is the App's sticky word on the connection, which the
    /// connection card says itself while it is up.
    @Published private(set) var toastSticky = false
    /// The App says its connection is down and it is redialing.
    @Published private(set) var reconnecting = false
    /// A redial just begun by itself: for its first moments nothing says
    /// so, and one back within them -- the usual one, on the way back to
    /// the app -- leaves the terminal as it was, without a word.
    @Published private(set) var redialQuiet = false
    private var redialSpoken: DispatchWorkItem?
    /// How long a redial goes unsaid.
    private static let quietRedial: TimeInterval = 2.5
    /// Dialled again on the way back from the background: for its first
    /// moments the last frame stays in sight with a line over its top, and
    /// the connection card waits. A failure, or the pane, ends it at once.
    @Published private(set) var cardHeld = false
    private var cardRelease: DispatchWorkItem?
    /// How long the card waits for a dial back from the background.
    private static let cardHold: TimeInterval = 3.0
    private var toastTimer: Timer?
    /// The terminal's background, as the App paints it: the chrome around
    /// the terminal takes the same colour.
    @Published var background = Color(white: 0.11) {
        didSet { ink = Self.ink(over: background) }
    }
    /// The chrome's ink over `background`: white over a dark scheme, near
    /// black over a light one.
    @Published private(set) var ink = Color.white
    /// Whether `background` is a dark one, which `ink` is white over.
    var dark: Bool { ink == .white }

    private static func ink(over color: Color) -> Color {
        var (r, g, b, a): (CGFloat, CGFloat, CGFloat, CGFloat) = (0, 0, 0, 0)
        guard UIColor(color).getRed(&r, green: &g, blue: &b, alpha: &a) else { return .white }
        return 0.2126 * r + 0.7152 * g + 0.0722 * b > 0.6 ? Color(white: 0.1) : .white
    }
    /// The App's font size in points, from its layout view.
    @Published private(set) var fontPt: Double = 11
    /// The focused pane's place in its scrollback: rows above the bottom,
    /// and rows there are; changes for a moment show the scrollbar.
    @Published private(set) var scroll: (Double, Int) = (0, 0)
    @Published private(set) var scrollShown = false
    private var scrollTimer: Timer?
    /// The background task that holds the connection after the app leaves
    /// the screen, for as long as iOS allows.
    private var backgroundTask: UIBackgroundTaskIdentifier = .invalid
    @Published var animating = false
    /// The log view is open.
    var logShown = false
    /// The log view is open or developer mode is on: the stats are worth
    /// refreshing.
    var wantsStats: Bool { logShown || settings.devMode }
    /// The views last read from the core.
    private var lastViews: ViewSet?
    /// The focused pane's rows for the text input, kept current by the
    /// core while they are wanted (the keyboard is up, or a selection).
    private(set) var screenWanted = false
    /// A swipe from the screen's left edge pulls the sidebar out (a narrow
    /// screen, the drawer shut): a flick that starts there is not a tab's.
    var edgePullsDrawer = false
    private(set) var screenJSON = "null"
    private var screenRevision: UInt64 = 0
    /// This connection's terminal is on screen, and the app in front: the
    /// core is told, so a session behind the screen takes nothing.
    private var onScreen = false
    private var inFront = true
    /// The app is in front (not in the background).
    var isInFront: Bool { inFront }
    /// The terminal's screen shows this host.
    var isOnScreen: Bool { onScreen }
    /// A list shows this host's threads while its terminal is off screen
    /// (`setViewsListed`): the views are read for it as the core says they
    /// changed, a few times a second at most, and not in the background,
    /// where nobody sees the list.
    private var listed = false
    private var listRefreshQueued = false
    /// The preferences, followed as they change.
    let settings = AppSettings.shared
    var smoothScroll: Bool { settings.smoothScroll }
    private var subscriptions: [AnyCancellable] = []
    /// The App's cell height in points, from its layout view.
    private(set) var cellHeight: Double = 20

    let core: Core
    /// The host this screen shows; edited in place from the failure card.
    @Published var host: Host? {
        didSet {
            if oldValue?.autoConnect != host?.autoConnect { applyAutoReconnect() }
        }
    }

    var autoReconnect: Bool { (host?.autoConnect ?? true) && settings.autoReconnect }

    private func applyAutoReconnect() {
        core.setSetting(key: "auto-reconnect", value: autoReconnect ? "true" : "false")
    }
    /// A key to approve: the host's own, or (`jump`) that of the jump host
    /// it is reached through, whose address and id it then has.
    struct HostKeyChallenge: Identifiable {
        let id: String
        let hostname: String
        let port: Int
        let hostID: UUID?
        let fingerprint: String
        var jump = false
        /// The jump host's name, for the question.
        var jumpName = ""
    }
    @Published var pendingHostKey: HostKeyChallenge?
    private var activeRequestID: String?
    private var requestedEndpoint: (hostname: String, port: Int, hostID: UUID?)?
    /// The jump host as the request dialled it, if it went through one.
    private var requestedJump: Host?

    weak var store: HostStore?
    weak var inputView: TerminalInputView?
    var cursorRect = CGRect(x: 8, y: 8, width: 2, height: 20)

    private let frameNeeded = AtomicFlag()
    private var displayLink: CADisplayLink?
    /// The display link rests while there is nothing to draw or read: up
    /// while it rests, and taken by whatever wakes it (`wakeLink`). Every
    /// tick otherwise wakes the main thread at the display's rate, a frame
    /// or not, which is battery spent on a terminal sitting still.
    private let linkResting = AtomicFlag()
    /// Ticks in a row that found nothing to do (`tick`).
    private var idleTicks = 0
    /// About half a second at 60 Hz: past a passing pause between frames.
    private static let idleTicksBeforeRest = 30
    private var statsTimer: Timer?
    private var logLines: [String] = []
    private var hostKeyToRemember: String?

    weak var layer: CAMetalLayer?
    private(set) var generation: UInt64 = 0
    private var wantsSurface = false
    private var lastSize: (Int, Int, Double) = (0, 0, 0)

    #if DEBUG
    // The probe's ssh setup, for the automated flows: a throwaway key and
    // a wrapper that points the remote proxy at an isolated HOME.
    private var probeKeyPath = "/tmp/ttp-ssh/userkey"
    private var probeRemoteCommand = "/tmp/ttp-ssh/thinkterm-remote cli --prefer-mux proxy"
    private var probeUser = probeUserName()
    // The acceptance flow's requests answered, and its stall gauge.
    private var acceptTimer: Timer?
    private var acceptAnswered = 0
    private var measuringTicks = false
    private var lastTickAt: CFTimeInterval = 0
    private var longestTickGap: CFTimeInterval = 0
    #endif

    init(host: Host?, store: HostStore?) {
        self.host = host
        self.store = store
        let sink = NotifySink()
        core = Core(notify: sink)
        sink.model = self

        let args = ProcessInfo.processInfo.arguments
        func arg(_ name: String) -> String? {
            guard let i = args.firstIndex(of: name), i + 1 < args.count else { return nil }
            return args[i + 1]
        }
        #if DEBUG
        if let u = arg("--user") { probeUser = u }
        if let k = arg("--key") { probeKeyPath = k }
        if let c = arg("--remote-command") { probeRemoteCommand = c }
        #endif

        let link = CADisplayLink(target: self, selector: #selector(tick))
        // At rest until the terminal is on screen (`enterScreen`): a host
        // the list connected by itself is drawn by nobody.
        link.isPaused = true
        link.add(to: .main, forMode: .common)
        displayLink = link
        // Every published change redraws the screen (and closes an open
        // menu): the stats tick only while the log shows them.
        statsTimer = Timer.scheduledTimer(withTimeInterval: 0.5, repeats: true) { [weak self] _ in
            guard let self, self.wantsStats else { return }
            let stats = self.core.stats()
            if stats != self.stats { self.stats = stats }
        }
        #if DEBUG
        if let name = arg("--scheme") { settings.schemeName = name }
        #endif
        applyScrollMode()
        applyScheme()
        // A setting publishes before it is stored: each of these reads the
        // settings again, so it waits a turn for the new value to be there.
        settings.$schemeName.dropFirst().receive(on: DispatchQueue.main).sink { [weak self] _ in self?.applyScheme() }.store(in: &subscriptions)
        settings.$smoothScroll.dropFirst().receive(on: DispatchQueue.main).sink { [weak self] _ in self?.applyScrollMode() }.store(in: &subscriptions)
        applyTerminalPrefs()
        settings.$cursorStyle.dropFirst().receive(on: DispatchQueue.main).sink { [weak self] _ in self?.applyTerminalPrefs() }.store(in: &subscriptions)
        settings.$cursorBlink.dropFirst().receive(on: DispatchQueue.main).sink { [weak self] _ in self?.applyTerminalPrefs() }.store(in: &subscriptions)
        settings.$contrast.dropFirst().receive(on: DispatchQueue.main).sink { [weak self] _ in self?.applyTerminalPrefs() }.store(in: &subscriptions)
        settings.$resizeMode.dropFirst().receive(on: DispatchQueue.main).sink { [weak self] _ in self?.applyTerminalPrefs() }.store(in: &subscriptions)
        settings.$autoReconnect.dropFirst().receive(on: DispatchQueue.main).sink { [weak self] _ in self?.applyTerminalPrefs() }.store(in: &subscriptions)
        settings.$paneBars.dropFirst().receive(on: DispatchQueue.main).sink { [weak self] _ in self?.applyTerminalPrefs() }.store(in: &subscriptions)
        settings.$language.dropFirst().receive(on: DispatchQueue.main).sink { [weak self] _ in self?.applyTerminalPrefs() }.store(in: &subscriptions)
        // A face is shaped at connect time: the connection is made again.
        settings.$fontFamily.dropFirst().removeDuplicates().receive(on: DispatchQueue.main).sink { [weak self] _ in
            guard let self, self.host?.autoConnect != false, self.connection.hasPrefix("pane ") else { return }
            self.disconnect()
            self.connect()
        }.store(in: &subscriptions)
        // The size, though, every pane takes live, from either settings page.
        applyFontSize(settings.fontSize)
        settings.$fontSize.dropFirst().removeDuplicates().sink { [weak self] pt in self?.applyFontSize(pt) }.store(in: &subscriptions)
        #if DEBUG
        if args.contains("--autotest") { scheduleAutotest() }
        if args.contains("--autoconnect") { scheduleAutoconnect() }
        if args.contains("--imetest") { scheduleImeTest() }
        if args.contains("--seltest") { scheduleSelectionTest() }
        if args.contains("--treetest") { scheduleTreeTest() }
        if args.contains("--uitest") { scheduleUiTest() }
        if args.contains("--accepttest") { scheduleAcceptTest() }
        if args.contains("--uploadtest") { scheduleUploadTest() }
        #endif
        // Off screen until its screen says otherwise: a host connected for
        // the list alone must not claim the desktop's tab, move its focus
        // or mark its threads seen.
        updateShown()
    }

    /// The sidebar's "+": the screen asks for the folder.
    func askName() { nameAsks += 1 }

    /// The screen went away but the connection stays: the frame loop
    /// rests until the screen is back. The surface goes with the view.
    func leaveScreen() {
        // Its text input goes with it: nothing reads the rows.
        setScreenWanted(false)
        displayLink?.isPaused = true
        // Off screen it is paused, not resting: nothing wakes it.
        _ = linkResting.take()
        onScreen = false
        updateShown()
        queueListRefresh()
    }

    func enterScreen() {
        _ = linkResting.take()
        idleTicks = 0
        displayLink?.isPaused = false
        onScreen = true
        updateShown()
    }

    private func updateShown() { core.setShown(shown: onScreen && inFront) }

    /// The list of every host's threads shows this one's, or stopped:
    /// while it does the core keeps the views current with the terminal
    /// off screen, and the list reads them (`refreshViews`).
    ///
    /// The core attaches to the server -- and has threads to list -- only
    /// once it has a surface to draw on, so a host listed while its
    /// terminal is off screen is lent one of the screen's size that
    /// nothing shows. The terminal's own takes its place when it comes
    /// (`surfaceReady`), and it is taken back when the list goes.
    func setViewsListed(_ listed: Bool) {
        self.listed = listed
        core.setViewsListed(listed: listed)
        queueListRefresh()
        if listed, generation == 0 {
            let screen = UIApplication.shared.connectedScenes
                .compactMap { ($0 as? UIWindowScene)?.screen }.first
            let scale = Double(screen?.scale ?? 3)
            let points = screen?.bounds.size ?? CGSize(width: 390, height: 844)
            let width = Int((Double(points.width) * scale).rounded())
            let height = Int((Double(points.height) * scale).rounded())
            let standIn = listSurface ?? CAMetalLayer()
            standIn.contentsScale = CGFloat(scale)
            standIn.bounds = CGRect(origin: .zero, size: points)
            standIn.drawableSize = CGSize(width: width, height: height)
            listSurface = standIn
            attach(standIn, width: width, height: height, scale: scale)
            onListSurface = generation != 0
        } else if !listed, onListSurface {
            onListSurface = false
            detach()
        }
    }

    /// The surface lent for the list, and whether the core draws on it.
    private var listSurface: CAMetalLayer?
    private var onListSurface = false

    /// From the core thread: the views changed. On screen the frame loop
    /// reads them; off it, a list may be.
    fileprivate func coreChanged() {
        // New views: the tick reads them.
        wakeLink()
        DispatchQueue.main.async { [weak self] in self?.queueListRefresh() }
    }

    /// The list's read of the views, soon and once for a burst of changes.
    private func queueListRefresh() {
        guard listed, !onScreen, inFront, !listRefreshQueued else { return }
        listRefreshQueued = true
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.25) { [weak self] in
            guard let self else { return }
            self.listRefreshQueued = false
            guard self.listed, !self.onScreen, self.inFront else { return }
            self.refreshViews()
        }
    }

    func shutdown() {
        clearHostKeyRequest()
        displayLink?.invalidate()
        statsTimer?.invalidate()
        // Uploads end with the core: their copies go now, as nothing reads
        // them, and nothing is asked of it after.
        uploadTimer?.invalidate()
        uploadTimer = nil
        for upload in uploads where upload.ended == nil {
            try? FileManager.default.removeItem(at: upload.file.deletingLastPathComponent())
        }
        detach()
        core.disconnect()
        core.shutdown()
    }

    // MARK: connection

    /// The chosen face first, the symbols fallback after it.
    var fontPaths: [String] {
        [terminalFace, "SymbolsNerdFontMono-Regular"].compactMap {
            Bundle.main.path(forResource: $0, ofType: "ttf")
        }
    }

    /// This install's name to the servers, made once: the tab this phone
    /// holds stays its own across launches.
    static let deviceId: String = {
        if let id = UserDefaults.standard.string(forKey: "client.id") { return id }
        let id = UUID().uuidString
        UserDefaults.standard.set(id, forKey: "client.id")
        return id
    }()

    /// A host as edited, onto its session. A changed connection that was up
    /// is made again on the edit, as is one the terminal's own Save asks
    /// for; that Save with nothing changed retries one that is down. Each
    /// is the user's tap or a session they had open, not a connection made
    /// by itself, so the host's `autoConnect` has no say in it. One lost and
    /// left for the user's retry is down, though its status still names the
    /// pane: an edit elsewhere is not that retry.
    func applyHostEdit(_ saved: Host, credentialsChanged: Bool, connectOnSave: Bool) {
        let changed = host?.hostname != saved.hostname || host?.port != saved.port
            || host?.user != saved.user || host?.auth != saved.auth
            || host?.remoteCommand != saved.remoteCommand || host?.knownHost != saved.knownHost
            || host?.jumpHost != saved.jumpHost || credentialsChanged
        let lost = reconnecting && !autoReconnect
        let needsRetry: Bool
        switch ConnectionPhase(status: connection) {
        case .failed, .disconnected: needsRetry = true
        case nil: needsRetry = lost
        default: needsRetry = false
        }
        let redial = changed ? ((isConnected && !lost) || connectOnSave) : (connectOnSave && needsRetry)
        if changed { disconnect() }
        host = saved
        if redial { connect() }
    }

    /// Connect to the model's host with the secret from the Keychain.
    func connect() {
        // A dial of its own: whoever holds the card back holds it for this
        // one, after the call (`reattachAfterBackground`).
        releaseCard()
        restoredThread = false
        // A new connection's views are all new, even ones that read the
        // same: the first listing puts the phone back on its thread. The
        // core may still have the last connection's out until it gets here.
        lastViews = nil
        connectionBefore = core.viewsConnection()
        guard let host else {
            // Only the automated flows open a screen with no host.
            #if DEBUG
            connectProbe()
            #endif
            return
        }
        // One whose jump host is gone is not dialled some other way.
        let jump = store?.jump(of: host)
        if host.jumpHost != nil, jump == nil {
            clearHostKeyRequest()
            connection = "failed: " + tr("jump.missing")
            return
        }
        let account = host.id.uuidString
        let secret = Keychain.load(account: account) ?? ""
        let passphrase = Keychain.load(account: account + ".passphrase")
        log("shell: \(host.auth == .password ? "password" : "key text") \(secret.count) chars from the Keychain" + (passphrase == nil ? "" : ", with a passphrase") + (jump.map { " via \($0.address)" } ?? ""))
        connect(
            hostname: host.hostname, port: host.port, user: host.user,
            authKind: host.auth == .password ? "password" : "key", secret: secret,
            passphrase: passphrase, knownHost: host.knownHost, remoteCommand: host.remoteCommand,
            jump: jump
        )
    }

    #if DEBUG
    /// The probe's connection: the private sshd on this Mac.
    func connectProbe() {
        connect(
            hostname: "127.0.0.1", port: 2299, user: probeUser, authKind: "key-path",
            secret: probeKeyPath, passphrase: nil, knownHost: hostKeyToRemember, remoteCommand: probeRemoteCommand
        )
    }
    #endif

    private func connect(hostname: String, port: Int, user: String, authKind: String, secret: String, passphrase: String?, knownHost: String?, remoteCommand: String, jump: Host? = nil) {
        clearHostKeyRequest()
        let requestID = UUID().uuidString
        activeRequestID = requestID
        requestedEndpoint = (hostname, port, host?.id)
        requestedJump = jump
        // The jump host signs in with its own login, from its own items.
        let through = jump.map { jump -> JumpHost in
            let account = jump.id.uuidString
            return JumpHost(
                host: jump.hostname, port: UInt16(clamping: jump.port), user: jump.user,
                authKind: jump.auth == .password ? "password" : "key",
                secret: Keychain.load(account: account) ?? "",
                passphrase: Keychain.load(account: account + ".passphrase"),
                knownHost: jump.knownHost
            )
        }
        let paths = fontPaths
        guard paths.count == 2 else {
            log("shell: fonts missing from the bundle (\(paths))")
            return
        }
        core.connect(
            requestId: requestID,
            host: hostname, port: UInt16(clamping: port), user: user,
            authKind: authKind, secret: secret, passphrase: passphrase, knownHost: knownHost,
            jump: through,
            remoteCommand: remoteCommand, deviceId: Self.deviceId, keepaliveSecs: UInt32(max(settings.keepAliveSeconds, 0)),
            fontPaths: paths, sizePt: settings.fontSize,
            painter: CoreTextPainter()
        )
    }

    private func clearHostKeyRequest() {
        activeRequestID = nil
        requestedEndpoint = nil
        requestedJump = nil
        pendingHostKey = nil
    }

    /// The jump host this request dialled, as the store has it now -- with
    /// the key pinned for it since, if one was -- while it is reached the
    /// same way.
    private func currentJump(_ requestID: String) -> Host? {
        guard activeRequestID == requestID, let asked = requestedJump,
              let now = store?.hosts.first(where: { $0.id == asked.id }),
              now.hostname == asked.hostname, now.port == asked.port,
              now.user == asked.user, now.auth == asked.auth else { return nil }
        return now
    }

    func disconnect() {
        releaseCard()
        clearHostKeyRequest()
        core.disconnect()
    }

    /// The host's pinned key forgotten: the connection up now stays, and
    /// the next dial -- the core's own redial of this one too -- asks for
    /// the key the host has now.
    func forgetHostKey() {
        host?.knownHost = nil
        core.forgetHostKey()
    }

    /// As `forgetHostKey`, for the jump host this one is reached through.
    func forgetJumpHostKey() {
        requestedJump?.knownHost = nil
        core.forgetJumpHostKey()
    }

    /// The failure card's "forget the key and retry": the key that changed
    /// is the jump host's when the failure says so. Either way it goes from
    /// every session that holds it, the ones reached through it too.
    func forgetKeyAndRetry(reason: String, sessions: Sessions) {
        guard let store, let host else { return connect() }
        if let jump = store.jump(of: host), reason.contains("the jump host") {
            sessions.forgetHostKey(jump.id, store: store)
        } else {
            sessions.forgetHostKey(host.id, store: store)
            self.host?.knownHost = nil
        }
        connect()
    }

    /// The jump host this host is reached through was edited: a session up
    /// through it is made again the new way, as one is on an edit of its
    /// own host. One lost and left for the user's retry stays so.
    func applyJumpHostEdit() {
        let lost = reconnecting && !autoReconnect
        let redial = isConnected && !lost
        disconnect()
        if redial { connect() }
    }

    func approveHostKey(_ challenge: HostKeyChallenge) {
        if challenge.jump {
            guard let jump = currentJump(challenge.id), jump.id == challenge.hostID,
                  jump.hostname == challenge.hostname, jump.port == challenge.port else { return }
            // One pinned meanwhile, in another session, is the one dialled
            // with: a key it does not match is the failure's to say.
            if jump.knownHost == nil { store?.rememberHostKey(challenge.fingerprint, for: jump.id) }
            connect()
            return
        }
        guard activeRequestID == challenge.id,
              requestedEndpoint?.hostname == challenge.hostname,
              requestedEndpoint?.port == challenge.port,
              host?.id == challenge.hostID else { return }
        if var host = host {
            guard host.hostname == challenge.hostname, host.port == challenge.port,
                  host.knownHost == nil else { return }
            host.knownHost = challenge.fingerprint
            store?.rememberHostKey(challenge.fingerprint, for: host.id)
            self.host = host
        } else {
            hostKeyToRemember = challenge.fingerprint
        }
        connect()
    }


    // MARK: input

    func key(_ name: String, ctrl: Bool = false, alt: Bool = false, shift: Bool = false) {
        // A sticky modifier is spent by the next key whether or not that
        // key brought the modifier itself, so it never lingers past it.
        let stickyCtrl = takeSticky(\.ctrlSticky)
        let stickyAlt = takeSticky(\.altSticky)
        inputView?.lineDisturbed()
        core.key(name: name, ctrl: ctrl || stickyCtrl, alt: alt || stickyAlt, shift: shift)
    }

    /// Text from the keyboard. A sticky Ctrl or Alt turns a single
    /// character into a chord.
    func text(_ text: String) {
        if (ctrlSticky || altSticky), text.count == 1 {
            key(text)
            return
        }
        inputView?.lineDisturbed()
        core.text(text: text)
    }

    func pasteFromClipboard() {
        if let text = UIPasteboard.general.string, !text.isEmpty {
            inputView?.lineDisturbed()
            core.paste(text: text)
        }
    }

    private func takeSticky(_ path: ReferenceWritableKeyPath<TerminalModel, Bool>) -> Bool {
        let on = self[keyPath: path]
        if on { self[keyPath: path] = false }
        return on
    }

    func pointer(_ kind: String, at point: CGPoint) {
        core.pointer(kind: kind, x: point.x, y: point.y)
    }

    func wheel(at point: CGPoint, lines: Double) {
        core.wheel(x: point.x, y: point.y, lines: lines)
    }

    func wheelPx(at point: CGPoint, px: Double) {
        core.wheelPx(x: point.x, y: point.y, px: px)
    }

    private func showToast(_ toast: Toast?) {
        toastTimer?.invalidate()
        let sticky = toast?.sticky ?? false
        if reconnecting != sticky {
            reconnecting = sticky
            redialSpoken?.cancel()
            redialQuiet = sticky && autoReconnect
            if redialQuiet {
                let spoken = DispatchWorkItem { [weak self] in self?.redialQuiet = false }
                redialSpoken = spoken
                DispatchQueue.main.asyncAfter(deadline: .now() + Self.quietRedial, execute: spoken)
            }
        }
        guard let toast, !toast.text.isEmpty else {
            if toastText != nil { toastText = nil }
            return
        }
        toastText = toast.text
        toastSticky = sticky
        if !sticky {
            toastTimer = Timer.scheduledTimer(withTimeInterval: 4, repeats: false) { [weak self] _ in
                guard let self, self.toastText == toast.text else { return }
                self.toastText = nil
            }
        }
    }

    /// A passing remark of the shell's own, where the App's go.
    func note(_ text: String) {
        toastTimer?.invalidate()
        toastText = text
        toastSticky = false
        toastTimer = Timer.scheduledTimer(withTimeInterval: 4, repeats: false) { [weak self] _ in
            guard let self, self.toastText == text else { return }
            self.toastText = nil
        }
    }

    // MARK: uploads

    /// Leave a picture in hand (a photo, the clipboard's) in the host's
    /// uploads folder and put its path on the focused pane's command
    /// line, as the desktop does with a file dropped on a remote terminal.
    /// `begun` is the upload as it was chosen (`beginUpload`), for one that
    /// took a while to load; none, begun now.
    func upload(name: String, data: Data, begun: UInt64? = nil) {
        let id = begun ?? beginUpload()
        guard let file = Self.uploadFile(named: name), (try? data.write(to: file)) != nil else {
            core.cancelUpload(id: id)
            return note(tr("upload.unread", name))
        }
        startUpload(id: id, name: name, file: file)
    }

    /// An upload begun as its file is chosen: its path goes into the pane
    /// focused now, however long the file takes to be ready.
    func beginUpload() -> UInt64 {
        core.uploadBegin()
    }

    /// The same for a file elsewhere (the Files app's): copied into the
    /// app's own room while its door is held, so the upload reads it at
    /// its own pace. Off the main thread, as a large file takes a while;
    /// and as a coordinated read, which has one in iCloud and not on the
    /// phone yet fetched first.
    func upload(copying url: URL) {
        let name = url.lastPathComponent
        let id = beginUpload()
        guard let file = Self.uploadFile(named: name) else {
            core.cancelUpload(id: id)
            return note(tr("upload.unread", name))
        }
        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            var copied = false
            do {
                let scoped = url.startAccessingSecurityScopedResource()
                defer { if scoped { url.stopAccessingSecurityScopedResource() } }
                var refused: NSError?
                NSFileCoordinator().coordinate(readingItemAt: url, options: .withoutChanges, error: &refused) { readable in
                    copied = (try? FileManager.default.copyItem(at: readable, to: file)) != nil
                }
            }
            DispatchQueue.main.async {
                guard let self, copied else {
                    try? FileManager.default.removeItem(at: file.deletingLastPathComponent())
                    self?.core.cancelUpload(id: id)
                    self?.note(tr("upload.unread", name))
                    return
                }
                self.startUpload(id: id, name: name, file: file)
            }
        }
    }

    /// Give an upload up: the host keeps nothing of it.
    func cancelUpload(_ id: UInt64) {
        core.cancelUpload(id: id)
    }

    private static var uploadsFolder: URL {
        FileManager.default.temporaryDirectory.appendingPathComponent("uploads", isDirectory: true)
    }

    /// What a process before this one left in the uploads folder, stopped
    /// in the middle of an upload: cleared once, before this one's first
    /// copy is made there.
    private static let clearedUploads: Void = {
        try? FileManager.default.removeItem(at: uploadsFolder)
    }()

    /// A place of the app's own for a file on its way, in a folder to
    /// itself so the name is the file's.
    private static func uploadFile(named name: String) -> URL? {
        _ = clearedUploads
        let folder = uploadsFolder.appendingPathComponent(UUID().uuidString, isDirectory: true)
        guard (try? FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)) != nil else { return nil }
        return folder.appendingPathComponent(name.isEmpty ? "file" : name)
    }

    private func startUpload(id: UInt64, name: String, file: URL) {
        core.uploadFile(id: id, name: name, path: file.path)
        uploads.append(Upload(id: id, name: name, file: file))
        guard uploadTimer == nil else { return }
        uploadTimer = Timer.scheduledTimer(withTimeInterval: 0.15, repeats: true) { [weak self] _ in self?.pollUploads() }
    }

    /// How each upload stands, as the core has it; one that is done pastes
    /// its path and stays a moment to say so, one that failed a little
    /// longer to say why, and one given up goes at once.
    private func pollUploads() {
        for index in uploads.indices where uploads[index].ended == nil {
            guard let status = core.uploadStatus(id: uploads[index].id) else {
                try? FileManager.default.removeItem(at: uploads[index].file.deletingLastPathComponent())
                uploads[index].ended = Date()
                continue
            }
            (uploads[index].sent, uploads[index].total) = (status.sent, status.total)
            guard status.done else { continue }
            uploads[index].ended = Date()
            try? FileManager.default.removeItem(at: uploads[index].file.deletingLastPathComponent())
            if status.error.isEmpty {
                uploads[index].path = status.path
                inputView?.lineDisturbed()
                // Into the pane it was started from, not whichever has
                // the focus now.
                core.pasteUpload(id: uploads[index].id, text: status.paste)
            } else {
                uploads[index].error = status.error
            }
            onUploadEnded?(uploads[index])
        }
        let now = Date()
        uploads.removeAll { upload in
            guard let ended = upload.ended else { return false }
            if upload.error == "cancelled" { return true }
            return now.timeIntervalSince(ended) > (upload.error == nil ? 1.5 : 5)
        }
        if uploads.isEmpty {
            uploadTimer?.invalidate()
            uploadTimer = nil
        }
    }

    /// The phone is in its light appearance: read from the screen, which
    /// the window's own appearance (the terminal's, `TerminalScreen`)
    /// does not change.
    private static var systemLight: Bool {
        UIScreen.main.traitCollection.userInterfaceStyle == .light
    }
    /// The appearance the scheme was last chosen for.
    private var schemeForLight: Bool?

    /// The appearance may have changed while the app was away.
    func appearanceMayHaveChanged() {
        if schemeForLight != Self.systemLight { applyScheme() }
    }

    /// The scheme the user picked; with none picked, the one that goes
    /// with the side the phone is on, as the desktop decides its own
    /// (`native_settings::effective_color_scheme`): light gets the light
    /// half of the default, dark the host's own.
    private func applyScheme() {
        let schemeName = settings.schemeName
        let light = Self.systemLight
        schemeForLight = light
        if schemeName == Schemes.followDesktop || Schemes.json(named: schemeName) == nil {
            core.setSetting(key: "terminal-scheme", value: "\"desktop\"")
            core.setPalette(scheme: nil)
            // On a light phone the light default stands in for a host's
            // scheme that is no choice at all, as the desktop decides its
            // own (`native_settings::resolve_color_scheme`): a scheme the
            // host's configuration names is kept on either side.
            let lightScheme = light ? Schemes.json(named: Schemes.lightDefault) : nil
            core.setFallbackPalette(
                scheme: lightScheme,
                unchosen: lightScheme == nil ? nil : Schemes.json(named: Schemes.darkDefault)
            )
        } else {
            core.setSetting(key: "terminal-scheme", value: "\"" + schemeName.replacingOccurrences(of: "\"", with: "\\\"") + "\"")
            core.setPalette(scheme: Schemes.json(named: schemeName))
            core.setFallbackPalette(scheme: nil, unchosen: nil)
        }
    }

    fileprivate func corePublished(_ key: String, _ value: String) {
        switch key {
        case "bg":
            guard let color = Color(hex: value) else { return }
            DispatchQueue.main.async {
                if self.background != color { self.background = color }
            }
        case "chat_picture":
            // Each small picture once, decoded here, off the main thread.
            struct Picture: Decodable { let pane: Int; let key: String; let data: String }
            guard let picture = ViewJSON.decode(Picture.self, value),
                  let bytes = Data(base64Encoded: picture.data),
                  let image = UIImage(data: bytes) else { return }
            DispatchQueue.main.async {
                if self.chatWith?.pane == picture.pane { self.chatThumbs[picture.key] = image }
            }
        case "chat":
            // Read here, off the main thread: a long chat is a long text.
            guard let chat = ViewJSON.decode(ChatState.self, value) else { return }
            // Only the chat on show: a late view of the one before is dropped.
            DispatchQueue.main.async {
                if self.chatWith?.pane == chat.pane { self.chat = chat }
            }
        default:
            return
        }
    }

    private func applyScrollMode() {
        core.setSetting(key: "scroll-mode", value: smoothScroll ? "\"smooth\"" : "\"stepped\"")
    }

    /// The preferences the shared App honours, as its JSON values.
    private func applyTerminalPrefs() {
        let style = ["auto", "block", "bar", "underline"].contains(settings.cursorStyle) ? settings.cursorStyle : "auto"
        core.setSetting(key: "cursor-style", value: "\"\(style)\"")
        core.setSetting(key: "cursor-blink", value: settings.cursorBlink ? "true" : "false")
        let contrast: Double = ["3": 3, "45": 4.5, "7": 7][settings.contrast] ?? 0
        core.setSetting(key: "min-contrast", value: String(contrast))
        core.setSetting(key: "resize-mode", value: settings.resizeMode == "release" ? "\"release\"" : "\"live\"")
        applyAutoReconnect()
        core.setSetting(key: "pane-bars", value: settings.paneBars ? "true" : "false")
        // The core's own menus and messages follow the app's language.
        core.setSetting(key: "language", value: "\"\(L10n.currentTag)\"")
    }

    /// The text size every pane takes, pinned: the settings' size, which a
    /// pinch steps (`stepFont`).
    private func applyFontSize(_ pt: Double) {
        core.setSetting(key: "font", value: "{\"mode\":\"pinned\",\"pt\":\(pt)}")
    }

    /// The tab `by` places along the strip from the current one, shown.
    func switchTab(by: Int) {
        guard let tabs = tabs?.tabs, !tabs.isEmpty,
              let at = tabs.firstIndex(where: { $0.current }) else { return }
        let next = at + by
        guard tabs.indices.contains(next) else { return }
        chromeClick("pane", pane: tabs[next].target)
    }

    /// The overview opened: the screens asked for are kept until it
    /// closes (`releaseScreens`).
    func keepScreens() { screensWanted = true }

    /// The whole tab a pane is in, as it is split, for a picture of it;
    /// `screens` fills in. Asked only while they are kept.
    func requestScreen(pane: Int) {
        guard screensWanted else { return }
        let now = ContinuousClock.now
        if let asked = screenAsks[pane], now - asked < .seconds(5) { return }
        screenAsks[pane] = now
        core.requestScreenPreview(pane: UInt32(pane))
    }

    /// The overview closed: its screens and pictures go, and answers on
    /// the way find nobody waiting.
    func releaseScreens() {
        screensWanted = false
        paintTurn += 1
        screens = [:]
        pictures = [:]
        screenSerials = [:]
        screenAsks = [:]
    }

    fileprivate func coreScreen(_ pane: Int, _ screen: TabPreview) {
        screenAsks[pane] = nil
        // One as it was is not drawn again.
        guard screensWanted, screens[pane] != screen else { return }
        screens[pane] = screen
        screenSerials[pane, default: 0] += 1
    }

    /// Draw a pane's screen `pixels` large off the main thread: `done` has
    /// it, unless the screens were let go meanwhile. Not kept: a flight to
    /// the terminal's size wants it once.
    func paint(pane: Int, pixels: CGSize, done: @escaping (CGImage) -> Void) {
        guard let screen = screens[pane] else { return }
        let face = terminalFace
        let turn = paintTurn
        painter.async { [weak self] in
            let picture = ScreenPainter.paint(screen, pixels: pixels, face: face)
            DispatchQueue.main.async {
                guard let self, self.screensWanted, turn == self.paintTurn, let picture else { return }
                done(picture)
            }
        }
    }

    /// Draw a pane's screen for its card; `pictures` has it once it is.
    func paintScreen(pane: Int, pixels: CGSize) {
        paint(pane: pane, pixels: pixels) { [weak self] picture in self?.pictures[pane] = picture }
    }

    /// The face the terminal draws in, by its PostScript name, which is
    /// its file's name too.
    var terminalFace: String { settings.fontFamily == "Fira Code" ? "FiraCode-Regular" : "JetBrainsMono-Regular" }

    /// A pane rang its bell: a buzz, if the setting says so.
    fileprivate func coreBell() {
        DispatchQueue.main.async {
            guard self.settings.bell else { return }
            UINotificationFeedbackGenerator().notificationOccurred(.warning)
        }
    }

    /// A pinch's step: a point on the text size every pane takes, kept as
    /// the settings' size, so the next connection and the next launch
    /// start at it too. (The core's own step, the desktop's Cmd+= for one
    /// pane, lives only as long as the connection; on a phone a pinch is
    /// the text size.)
    func stepFont(_ by: Double) {
        let size = min(max((settings.fontSize + by).rounded(), 6), 40)
        if size != settings.fontSize { settings.fontSize = size }
    }

    // MARK: the App's chrome

    func chromeClick(_ action: String, pane: Int? = nil, tab: Int? = nil) {
        // Another pane may have the line now: what the keyboard typed on
        // this one is not to be changed there (`lineDisturbed`).
        inputView?.lineDisturbed()
        core.chromeClick(action: action, pane: pane.map { UInt32($0) }, tab: tab.map { UInt32($0) })
    }

    /// One pane of a tab at a time, whole, where the screen has no room
    /// for a split's panes side by side.
    func setOnePane(_ one: Bool) { core.setOnePane(one: one) }

    func sideClick(_ kind: String, id: String? = nil, flag: Bool? = nil) {
        inputView?.lineDisturbed()
        core.sideClick(kind: kind, id: id, flag: flag)
    }

    func sideKey(_ key: String, value: String) {
        core.sideKey(key: key, value: value)
    }

    /// Show a thread, switching the Space on show to its own first when
    /// it is in another one, so the strips follow.
    /// The thread on show is written down for the next connect; the first
    /// listing after a connect puts the phone back on the one it left.
    private func rememberThread(_ threads: ThreadsView?) {
        guard let key = lastThreadKey, let list = threads?.threads,
              let current = list.first(where: { $0.current })?.id else { return }
        if !restoredThread {
            restoredThread = true
            if let saved = UserDefaults.standard.string(forKey: key), saved != current,
               list.contains(where: { $0.id == saved }) {
                openThread(saved, space: nil)
                return
            }
        }
        UserDefaults.standard.set(current, forKey: key)
    }

    func openThread(_ id: String, space: String?) {
        if let space, tree?.spaces.first(where: { $0.current })?.id != space { core.setSpace(id: space) }
        sideClick("thread", id: id)
    }

    func setSpace(_ id: String) { core.setSpace(id: id) }

    func contextMenu(_ kind: String, id: String) -> [MenuItem] {
        ViewJSON.decode([MenuItem].self, core.contextMenu(kind: kind, id: id)) ?? []
    }

    /// Run a menu row. Copy and paste are the shell's to do.
    func menuAction(_ id: String) {
        guard let outcome = ViewJSON.decode(MenuOutcome.self, core.menuAction(id: id)) else { return }
        if let text = outcome.copy {
            UIPasteboard.general.string = text
        }
        if outcome.paste {
            pasteFromClipboard()
        }
    }

    func takeOver() { core.takeOver() }

    /// Open or fold the sub-task list of the agent in `pane`.
    func toggleSubtasks(pane: Int) { core.agentToggleSubtasks(pane: UInt32(pane)) }

    // MARK: an agent's conversation as a chat

    /// Whom the chat on show is with: the pane, the agent as its detection
    /// names it, and where it is ("Project · Thread").
    struct ChatTarget: Equatable {
        let pane: Int
        let agent: String
        let place: String
    }

    /// The chat on show, over the terminal of its agent's pane; nil when
    /// none is (`chat` is nil too until its first view comes).
    @Published private(set) var chatWith: ChatTarget? { didSet { chatFeed.with = chatWith } }
    @Published private(set) var chat: ChatState? { didSet { chatFeed.chat = chat } }

    /// The chat's own state, apart from the rest of the model: what the
    /// chat page watches. The model changes with each refresh of the
    /// terminals (their screens, the stats), many times a second while an
    /// agent works; the chat page, over the agent's terminal, is redrawn
    /// only when the chat changes.
    final class ChatFeed: ObservableObject {
        @Published fileprivate(set) var with: ChatTarget?
        @Published fileprivate(set) var chat: ChatState?
        @Published fileprivate(set) var files: [ChatFile] = []
        @Published fileprivate(set) var pictures: [String: URL] = [:]
        @Published fileprivate(set) var thumbs: [String: UIImage] = [:]
    }

    let chatFeed = ChatFeed()

    /// Show `row`'s agent as a chat. The terminal under it goes to the
    /// agent's pane first, as a tap on the row always did: going back from
    /// the chat is going to that terminal.
    func openChat(_ row: AgentRow) {
        chromeClick("pane", pane: row.pane)
        chat = nil
        chatThumbs = [:]
        chatWith = ChatTarget(pane: row.pane, agent: row.agent_id, place: row.place.isEmpty ? row.name : row.place)
        core.chatOpen(pane: UInt32(row.pane), agent: row.agent_id)
    }

    func closeChat() {
        guard chatWith != nil else { return }
        chatWith = nil
        chat = nil
        core.chatClose()
        for file in chatFiles { chatDetach(file) }
    }

    // MARK: files sent in the chat

    /// A file picked to go with what is said next: on its way to the host's
    /// uploads folder, there (`path`), or not (`error`).
    struct ChatFile: Identifiable, Equatable {
        let id: UInt64
        let name: String
        let image: Bool
        /// Its size, in bytes.
        let size: Int64
        /// A small picture of it, for a picture.
        let preview: UIImage?
        /// The app's own copy, until it is sent.
        let file: URL
        var sent: UInt64 = 0
        var total: UInt64 = 0
        var path: String?
        var error: String?
    }

    /// What goes with what is said next.
    @Published private(set) var chatFiles: [ChatFile] = [] { didSet { chatFeed.files = chatFiles } }
    /// The phone's own copies of the pictures sent, by where they are on
    /// the host: what a tap shows big, and what shows before the host's
    /// small picture comes.
    @Published private(set) var chatPictures: [String: URL] = [:] { didSet { chatFeed.pictures = chatPictures } }
    /// The host's small pictures of what was sent, by the key the chat's
    /// rows name them by: each comes once ("chat_picture").
    @Published private(set) var chatThumbs: [String: UIImage] = [:] { didSet { chatFeed.thumbs = chatThumbs } }
    private var chatFilesTimer: Timer?

    /// Put what the phone has -- a photo, the clipboard's picture -- in the
    /// host's uploads folder, to go with what is said next.
    func chatAttach(name: String, data: Data, image: Bool) {
        let id = core.uploadBegin()
        guard let file = Self.chatFile(named: name), (try? data.write(to: file)) != nil else {
            core.cancelUpload(id: id)
            return note(tr("upload.unread", name))
        }
        startChatFile(id: id, name: name, image: image, file: file)
    }

    /// The same for a file elsewhere (the Files app's), copied as a
    /// coordinated read, off the main thread.
    func chatAttach(copying url: URL) {
        let name = url.lastPathComponent
        let id = core.uploadBegin()
        guard let file = Self.chatFile(named: name) else {
            core.cancelUpload(id: id)
            return note(tr("upload.unread", name))
        }
        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            var copied = false
            let scoped = url.startAccessingSecurityScopedResource()
            var refused: NSError?
            NSFileCoordinator().coordinate(readingItemAt: url, options: .withoutChanges, error: &refused) { readable in
                copied = (try? FileManager.default.copyItem(at: readable, to: file)) != nil
            }
            if scoped { url.stopAccessingSecurityScopedResource() }
            DispatchQueue.main.async {
                guard let self, copied else {
                    try? FileManager.default.removeItem(at: file.deletingLastPathComponent())
                    self?.core.cancelUpload(id: id)
                    self?.note(tr("upload.unread", name))
                    return
                }
                let kind = file.pathExtension.lowercased()
                // A HEIF made a JPEG, as a photo is; pictures agents read go
                // as pictures; anything else (an SVG, a TIFF) by its path.
                if kind == "heic" || kind == "heif" {
                    let jpeg = UIImage(contentsOfFile: file.path)?.jpegData(compressionQuality: 0.92)
                    try? FileManager.default.removeItem(at: file.deletingLastPathComponent())
                    guard let jpeg else {
                        self.core.cancelUpload(id: id)
                        self.note(tr("upload.unread", name))
                        return
                    }
                    self.core.cancelUpload(id: id)
                    self.chatAttach(name: (name as NSString).deletingPathExtension + ".jpg", data: jpeg, image: true)
                    return
                }
                let image = ["png", "jpg", "jpeg", "gif", "webp"].contains(kind)
                self.startChatFile(id: id, name: name, image: image, file: file)
            }
        }
    }

    /// Take a picked file off what goes next; one still on its way is
    /// given up.
    func chatDetach(_ file: ChatFile) {
        if file.path == nil && file.error == nil { core.cancelUpload(id: file.id) }
        chatFiles.removeAll { $0.id == file.id }
        try? FileManager.default.removeItem(at: file.file.deletingLastPathComponent())
    }

    private static var chatFilesFolder: URL {
        FileManager.default.temporaryDirectory.appendingPathComponent("chat-files", isDirectory: true)
    }

    /// What a process before this one left: cleared once, before the
    /// first copy is made.
    private static let clearedChatFiles: Void = {
        try? FileManager.default.removeItem(at: chatFilesFolder)
    }()

    private static func chatFile(named name: String) -> URL? {
        _ = clearedChatFiles
        let folder = chatFilesFolder.appendingPathComponent(UUID().uuidString, isDirectory: true)
        guard (try? FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)) != nil else { return nil }
        return folder.appendingPathComponent(name.isEmpty ? "file" : name.replacingOccurrences(of: "/", with: "_"))
    }

    private func startChatFile(id: UInt64, name: String, image: Bool, file: URL) {
        core.uploadFile(id: id, name: file.lastPathComponent, path: file.path)
        let size = (try? FileManager.default.attributesOfItem(atPath: file.path)[.size] as? NSNumber)?.int64Value ?? 0
        let preview = image ? UIImage(contentsOfFile: file.path)?.preparingThumbnail(of: CGSize(width: 240, height: 240)) : nil
        chatFiles.append(ChatFile(id: id, name: file.lastPathComponent, image: image, size: size, preview: preview, file: file))
        guard chatFilesTimer == nil else { return }
        chatFilesTimer = Timer.scheduledTimer(withTimeInterval: 0.15, repeats: true) { [weak self] _ in self?.pollChatFiles() }
    }

    private func pollChatFiles() {
        var going = false
        for index in chatFiles.indices where chatFiles[index].path == nil && chatFiles[index].error == nil {
            guard let status = core.uploadStatus(id: chatFiles[index].id) else {
                chatFiles[index].error = "cancelled"
                continue
            }
            (chatFiles[index].sent, chatFiles[index].total) = (status.sent, status.total)
            guard status.done else {
                going = true
                continue
            }
            // Its path goes with what is said, not into a pane.
            core.uploadUnpasted(id: chatFiles[index].id)
            if status.error.isEmpty {
                chatFiles[index].path = status.path
            } else {
                chatFiles[index].error = status.error
            }
        }
        if !going {
            chatFilesTimer?.invalidate()
            chatFilesTimer = nil
        }
    }

    /// Say `text`, with the files picked that have reached the host.
    func chatSend(_ text: String) {
        let ready = chatFiles.filter { $0.path != nil }
        for file in ready {
            if file.image, let path = file.path {
                chatPictures[path] = file.file
            } else {
                try? FileManager.default.removeItem(at: file.file.deletingLastPathComponent())
            }
        }
        chatFiles.removeAll { $0.path != nil }
        core.chatSend(text: text, attachments: ready.map { ChatAttachment(name: $0.name, image: $0.image, path: $0.path ?? "") })
    }

    /// A command picked from the field's list, sent as typed: the files
    /// picked wait for what is said next.
    func chatCommand(_ text: String) { core.chatSend(text: text, attachments: []) }
    func chatSetModel(_ model: String, effort: String?) { core.chatSetModel(model: model, effort: effort) }
    func chatAnswer(ask: String, choice: String) { core.chatAnswer(ask: ask, choice: choice) }
    func chatInterrupt() { core.chatInterrupt() }
    func chatOlder() { core.chatOlder() }
    func chatAction(_ action: String) { core.chatAction(action: action) }
    func chatToggle(_ row: String) { core.chatToggle(row: row) }

    /// The text between two visible cells of the focused pane, for the
    /// clipboard, as the core reads it from the cells; the pane's own
    /// selection is left be. A copy waits for the core.
    func copyText(from first: (row: Int, col: Int), to last: (row: Int, col: Int)) -> String? {
        core.textBetween(anchorRow: UInt32(first.row), anchorCol: UInt32(first.col),
                         headRow: UInt32(last.row), headCol: UInt32(last.col))
    }

    /// The text input reads the rows (the keyboard is up, or there is a
    /// selection): the core keeps them current while it does.
    func setScreenWanted(_ wanted: Bool) {
        guard wanted != screenWanted else { return }
        screenWanted = wanted
        if wanted {
            // What is out was made while nobody read it: the next made (the
            // core makes one at once) is read, and until then the rows are
            // asked for. Taken before asking, so that one is not missed.
            screenRevision = core.screenRevision()
            // Rows come without a word from the core: the tick reads them,
            // so it runs while they are wanted.
            wakeLink()
        } else {
            screenJSON = "null"
        }
        core.setScreenWanted(wanted: wanted)
    }

    /// Read the views the core last published. Called on the main thread
    /// each frame the revision moved; a burst of changes is one read, and
    /// it never waits for the core thread.
    func refreshViews() {
        // The count first: the views are copied only when they changed.
        guard core.viewsRevision() != lastViews?.revision else { return }
        let views = core.views()
        guard views.revision != lastViews?.revision else { return }
        let last = lastViews
        lastViews = views
        // A change of cards marks the views stale, so it is seen here.
        let icons = core.tabIconsRevision()
        if icons != iconsRevision { iconsRevision = icons }
        // Decoded only when the text changed, and assigned only on a
        // change: a published write redraws the whole screen, and the
        // core publishes as often as output comes.
        if views.tabs != last?.tabs {
            let tabs = ViewJSON.decode(TabsView.self, views.tabs)
            if tabs != self.tabs { self.tabs = tabs }
        }
        if views.sidebar != last?.sidebar {
            let sidebar = ViewJSON.decode(SidebarView.self, views.sidebar)
            if sidebar != self.sidebar { self.sidebar = sidebar }
        }
        let threadsChanged = views.threads != last?.threads
        if threadsChanged {
            let threads = ViewJSON.decode(ThreadsView.self, views.threads)
            if threads != self.threads { self.threads = threads }
        }
        if views.connection > connectionBefore, threadsChanged || !restoredThread {
            rememberThread(threads)
        }
        if views.tree != last?.tree {
            let tree = ViewJSON.decode(TreeView.self, views.tree)
            if tree != self.tree { self.tree = tree }
        }
        if views.navs != last?.navs {
            let navs = ViewJSON.decode([NavView].self, views.navs) ?? []
            if navs != self.navs { self.navs = navs }
        }
        if views.panes != last?.panes {
            let panes = ViewJSON.decode(PanesView.self, views.panes)
            if panes != self.panes { self.panes = panes }
        }
        if views.agents != last?.agents {
            let agents = ViewJSON.decode(AgentsView.self, views.agents)
            if agents != self.agents { self.agents = agents }
        }
        if views.status != last?.status {
            let status = ViewJSON.decode(StatusView.self, views.status)
            if status != self.status {
                self.status = status
                showToast(status?.toast)
            }
        }
        guard views.layout != last?.layout else { return }
        let layout = views.layout
        if let r = layout.range(of: "\"cell\":["),
           let end = layout[r.upperBound...].firstIndex(of: "]") {
            let parts = layout[r.upperBound..<end].split(separator: ",")
            if parts.count == 2, let h = Double(parts[1]), h > 0 { cellHeight = h }
        }
        if let r = layout.range(of: "\"scroll\":["),
           let end = layout[r.upperBound...].firstIndex(of: "]") {
            let parts = layout[r.upperBound..<end].split(separator: ",")
            if parts.count == 2, let a = Double(parts[0]), let b = Int(parts[1]), (a, b) != scroll {
                scroll = (a, b)
                scrollShown = true
                scrollTimer?.invalidate()
                scrollTimer = Timer.scheduledTimer(withTimeInterval: 1.0, repeats: false) { [weak self] _ in
                    self?.scrollShown = false
                }
            }
        }
        if let r = layout.range(of: "\"font_pt\":"),
           let end = layout[r.upperBound...].firstIndex(where: { $0 == "," || $0 == "}" }),
           let pt = Double(layout[r.upperBound..<end]), pt > 0, pt != fontPt {
            fontPt = pt
        }
    }

    // MARK: surface lifecycle, all on the main thread

    func surfaceReady(_ layer: CAMetalLayer, width: Int, height: Int, scale: Double) {
        if onListSurface {
            // The terminal's own surface, in the place of the list's.
            onListSurface = false
            detach()
        }
        self.layer = layer
        wantsSurface = true
        if generation == 0 {
            attach(layer, width: width, height: height, scale: scale)
        } else if lastSize != (width, height, scale) {
            lastSize = (width, height, scale)
            log("shell: surface \(width)x\(height) @\(scale)")
            core.resize(generation: generation, width: UInt32(width), height: UInt32(height), scale: scale)
        }
    }

    func surfaceGone() {
        wantsSurface = false
        detach()
    }

    private func attach(_ layer: CAMetalLayer, width: Int, height: Int, scale: Double) {
        let ptr = UInt64(UInt(bitPattern: Unmanaged.passUnretained(layer).toOpaque()))
        let gen = core.attachSurface(layer: ptr, width: UInt32(width), height: UInt32(height), scale: scale)
        generation = gen
        lastSize = (width, height, scale)
        attached = gen != 0
        if gen == 0 { log("shell: attach failed") }
    }

    private func detach() {
        guard generation != 0 else { return }
        let started = CACurrentMediaTime()
        core.detachSurface(generation: generation)
        let ms = (CACurrentMediaTime() - started) * 1000
        log(String(format: "shell: detach %d took %.2f ms", generation, ms))
        generation = 0
        attached = false
    }

    func toggleAttached() {
        if attached {
            detach()
        } else if let layer, let size = drawableSize(of: layer) {
            attach(layer, width: size.0, height: size.1, scale: size.2)
        }
    }

    /// The background took the connection down; the foreground brings it back.
    /// Meanwhile the connection page stays away: a "disconnected" of the
    /// app's own making is not one to show, in the app switcher's picture
    /// of it or as the app comes back (`TerminalScreen.connectionCovers`).
    @Published private(set) var pausedForBackground = false

    func detachForBackground() {
        inFront = false
        updateShown()
        detach()
        report("background")
        if settings.keepSessionInBackground {
            // iOS grants about half a minute; the socket lives that long.
            backgroundTask = UIApplication.shared.beginBackgroundTask(withName: "thinkterm-connection") { [weak self] in
                self?.endBackgroundTask()
            }
        } else if !connection.hasPrefix("idle"), !connection.hasPrefix("disconnected"), !connection.hasPrefix("failed") {
            // Attached or still on the way (a failed dial waiting to be made
            // again reads "connecting" too): torn down for the background,
            // and dialled again on the way back -- whatever the status
            // reads by then.
            pausedForBackground = true
            disconnect()
        }
    }

    /// See `cardHeld`.
    private func holdCard() {
        cardRelease?.cancel()
        cardHeld = true
        let release = DispatchWorkItem { [weak self] in self?.releaseCard() }
        cardRelease = release
        DispatchQueue.main.asyncAfter(deadline: .now() + Self.cardHold, execute: release)
    }

    private func releaseCard() {
        cardRelease?.cancel()
        cardRelease = nil
        if cardHeld { cardHeld = false }
    }

    private func endBackgroundTask() {
        if backgroundTask != .invalid {
            UIApplication.shared.endBackgroundTask(backgroundTask)
            backgroundTask = .invalid
        }
    }

    func reattachAfterBackground() {
        inFront = true
        updateShown()
        queueListRefresh()
        endBackgroundTask()
        if pausedForBackground {
            pausedForBackground = false
            if host?.autoConnect != false {
                connect()
                holdCard()
            }
        } else {
            // What the wait cut is found out now, and what was already
            // down is tried again at once.
            core.checkConnection()
        }
        guard wantsSurface, generation == 0, let layer, let size = drawableSize(of: layer) else { return }
        attach(layer, width: size.0, height: size.1, scale: size.2)
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.0) { self.report("foreground") }
    }

    func cycleAttachDetach(times: Int) {
        guard let layer, let size = drawableSize(of: layer) else { return }
        let started = CACurrentMediaTime()
        for _ in 0..<times {
            detach()
            attach(layer, width: size.0, height: size.1, scale: size.2)
        }
        let ms = (CACurrentMediaTime() - started) * 1000
        log(String(format: "shell: %d detach/attach cycles in %.1f ms", times, ms))
    }

    private func drawableSize(of layer: CAMetalLayer) -> (Int, Int, Double)? {
        let scale = Double(layer.contentsScale)
        let w = Int((Double(layer.bounds.width) * scale).rounded())
        let h = Int((Double(layer.bounds.height) * scale).rounded())
        guard w > 0, h > 0 else { return nil }
        return (w, h, scale)
    }

    // MARK: frames

    /// The display link: paint if asked, then read what the core published
    /// -- the counts say whether there is anything new, without a wait.
    @objc private func tick() {
        #if DEBUG
        if measuringTicks {
            let now = CACurrentMediaTime()
            if lastTickAt > 0 { longestTickGap = max(longestTickGap, now - lastTickAt) }
            lastTickAt = now
        }
        #endif
        // Rows read while wanted come without a word from the core, so a
        // tick that wants them is never idle.
        var busy = screenWanted
        if frameNeeded.take() {
            busy = true
            core.render()
            // Unwanted rows are not kept current: read again when asked.
            if !screenWanted { inputView?.dropScreen() }
        }
        if core.viewsRevision() != lastViews?.revision {
            busy = true
            refreshViews()
        }
        if screenWanted {
            let revision = core.screenRevision()
            if revision != screenRevision {
                screenRevision = revision
                screenJSON = core.screen()
                inputView?.screenChanged()
            }
        }
        #if DEBUG
        // The acceptance flow's stall gauge reads the gaps between ticks.
        if measuringTicks { busy = true }
        #endif
        if busy {
            idleTicks = 0
        } else {
            idleTicks += 1
            if idleTicks >= Self.idleTicksBeforeRest { restLink() }
        }
    }

    /// Nothing to draw or read for a while: the link stops until the core
    /// asks for a frame or publishes views (`wakeLink`).
    private func restLink() {
        idleTicks = 0
        displayLink?.isPaused = true
        linkResting.set()
        // A frame or views asked for between this tick's look and the flag
        // going up found the link awake and did not wake it: looked at
        // again now that the flag is up, so neither waits for the next.
        if frameNeeded.get() || core.viewsRevision() != lastViews?.revision {
            wakeLink()
        }
    }

    /// From any thread: the link resting is set going again, once.
    private func wakeLink() {
        guard linkResting.take() else { return }
        DispatchQueue.main.async { [weak self] in
            // Resting again by now (the screen left and came back, and the
            // link went idle meanwhile): that rest has a wake of its own.
            guard let self, self.onScreen, !self.linkResting.get() else { return }
            self.displayLink?.isPaused = false
        }
    }

    func requestOneFrame() {
        frameNeeded.set()
        wakeLink()
    }

    func toggleAnimating() {
        animating.toggle()
        core.setAnimating(on: animating)
    }

    func toggleInset() { inset = inset > 0 ? 0 : 60 }

    func focusKeyboard() { inputView?.becomeFirstResponder() }
    func showCopied() {
        copied = true
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.2) { self.copied = false }
    }
    func dismissKeyboard() { inputView?.resignFirstResponder() }
    func compositionChanged(_ text: String?) {
        DispatchQueue.main.async { self.composing = text }
    }

    // MARK: notify (core thread)

    fileprivate func coreWantsFrame() {
        frameNeeded.set()
        wakeLink()
    }

    fileprivate func coreStatus(_ status: String) {
        DispatchQueue.main.async {
            self.connection = status
            self.log("status: " + status)
            if status.hasPrefix("pane "), let host = self.host {
                self.store?.touchConnected(host.id)
            }
            // Back on the pane, or stopped: nothing is left to wait for.
            // A plain "disconnected" may be the background's own, landing
            // after the dial back began, so it does not end the wait.
            if status.hasPrefix("pane ") || ConnectionPhase(status: status)?.isFailure == true {
                self.releaseCard()
            }
            if status.hasPrefix("connecting") || status.contains("reconnect") || status.hasPrefix("disconnected") {
                self.inputView?.dropComposition()
            }
        }
    }

    fileprivate func coreTitle(_ title: String) {
        DispatchQueue.main.async {
            if self.title != title { self.title = title }
        }
    }

    fileprivate func coreHostKeyRequired(_ requestID: String, _ fingerprint: String, _ jump: Bool) {
        DispatchQueue.main.async {
            if jump {
                guard let through = self.currentJump(requestID) else { return }
                // Its key was pinned since this dial, in another session --
                // its own, or one through it: dialled again with it, rather
                // than asked about twice or left unasked.
                if through.knownHost != nil {
                    self.connect()
                    return
                }
                self.pendingHostKey = HostKeyChallenge(
                    id: requestID, hostname: through.hostname, port: through.port, hostID: through.id,
                    fingerprint: fingerprint, jump: true, jumpName: through.display)
                return
            }
            guard self.activeRequestID == requestID, let endpoint = self.requestedEndpoint,
                  self.host?.id == endpoint.hostID else { return }
            if let host = self.host {
                guard host.hostname == endpoint.hostname, host.port == endpoint.port,
                      host.knownHost == nil else { return }
                // Pinned since this dial, in a session reached through this
                // host: dialled again with it.
                if let pinned = self.store?.hosts.first(where: { $0.id == host.id })?.knownHost {
                    self.host?.knownHost = pinned
                    self.connect()
                    return
                }
            }
            let challenge = HostKeyChallenge(id: requestID, hostname: endpoint.hostname,
                port: endpoint.port, hostID: endpoint.hostID, fingerprint: fingerprint)
            #if DEBUG
            // The automated flows dial the probe's throwaway sshd, and no
            // one is there to read its fingerprint.
            if self.host == nil, Self.automated {
                self.approveHostKey(challenge)
                return
            }
            #endif
            self.pendingHostKey = challenge
        }
    }

    /// Launched for an automated flow (`--autoconnect`, `--uitest`, ...):
    /// the app opens straight on the probe's terminal and connects on the
    /// flow's schedule. Never in a release.
    static let automated: Bool = {
        #if DEBUG
        return ProcessInfo.processInfo.arguments.contains { $0.hasPrefix("--auto") || $0.hasSuffix("test") }
        #else
        return false
        #endif
    }()

    fileprivate func coreLogged(_ line: String) {
        print("core: " + line)
        DispatchQueue.main.async { self.log("core: " + line) }
    }

    private func log(_ line: String) {
        #if DEBUG
        NSLog("%@", line)
        #endif
        logLines.append(line)
        if logLines.count > 60 { logLines.removeFirst(logLines.count - 60) }
        logText = logLines.joined(separator: "\n")
    }

    // MARK: automated flows (launch arguments; results on the console)

    private func report(_ step: String) {
        let line = "AUTOTEST \(step): \(core.stats())"
        print(line)
        NSLog("%@", line)
        log(line)
    }

    #if DEBUG
    private func scheduleAutotest() {
        let q = DispatchQueue.main
        q.asyncAfter(deadline: .now() + 2.0) { self.report("start") }
        q.asyncAfter(deadline: .now() + 2.1) { self.toggleAnimating() }
        q.asyncAfter(deadline: .now() + 4.1) { self.toggleAnimating(); self.report("after 2s animating") }
        q.asyncAfter(deadline: .now() + 4.5) { self.cycleAttachDetach(times: 20); self.report("after 20 cycles") }
        q.asyncAfter(deadline: .now() + 5.0) { self.requestOneFrame() }
        q.asyncAfter(deadline: .now() + 5.5) { self.toggleInset() }
        q.asyncAfter(deadline: .now() + 6.5) { self.report("after shrink"); self.toggleInset() }
        q.asyncAfter(deadline: .now() + 7.5) { self.report("after grow"); self.detach() }
        q.asyncAfter(deadline: .now() + 8.0) { self.requestOneFrame(); self.report("detached, frame requested") }
        q.asyncAfter(deadline: .now() + 8.5) { self.toggleAttached() }
        q.asyncAfter(deadline: .now() + 9.5) { self.report("reattached"); print("AUTOTEST DONE") }
    }

    private func scheduleAutoconnect() {
        let q = DispatchQueue.main
        q.asyncAfter(deadline: .now() + 1.5) { self.connectProbe(); self.report("connect sent") }
        q.asyncAfter(deadline: .now() + 5.0) { self.report("after connect") }
        q.asyncAfter(deadline: .now() + 5.2) {
            self.core.text(text: "echo '你好, 世界 🙂 hello from iOS'")
            self.key("Enter")
        }
        q.asyncAfter(deadline: .now() + 7.5) { self.report("after typing") }
        q.asyncAfter(deadline: .now() + 7.6) {
            self.core.text(text: "sleep 2; echo WOKE-BY-OUTPUT")
            self.key("Enter")
        }
        q.asyncAfter(deadline: .now() + 8.6) { self.report("before wake") }
        q.asyncAfter(deadline: .now() + 11.0) { self.report("after wake") }
        q.asyncAfter(deadline: .now() + 11.2) { self.cycleAttachDetach(times: 5); self.report("after 5 cycles connected") }
        q.asyncAfter(deadline: .now() + 12.5) { self.report("final") }
        q.asyncAfter(deadline: .now() + 15.0) { self.report("during reconnect") }
        q.asyncAfter(deadline: .now() + 19.0) {
            self.core.text(text: "echo AFTER-RECONNECT")
            self.key("Enter")
        }
        q.asyncAfter(deadline: .now() + 21.0) { self.report("after reconnect"); print("AUTOCONNECT DONE") }
    }

    /// `--accepttest`: connect to the probe and do what ios/tools/accept.sh
    /// asks, the driver that cuts the connection, restarts the server and
    /// sends the app to the background from the Mac. Its requests come one
    /// at a time through a file the simulator shares with the Mac, as
    /// "<n> <verb> <argument>"; each is answered on the console with
    /// "ACCEPT ack <n> <detail>".
    private func scheduleAcceptTest() {
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.5) { self.connectProbe() }
        acceptTimer = Timer.scheduledTimer(withTimeInterval: 0.25, repeats: true) { [weak self] _ in
            self?.pollAcceptRequest()
        }
    }

    private static let acceptRequests = "/tmp/ttp-ssh/accept-request"

    private func pollAcceptRequest() {
        guard let text = try? String(contentsOfFile: Self.acceptRequests, encoding: .utf8) else { return }
        let parts = text.trimmingCharacters(in: .whitespacesAndNewlines)
            .split(separator: " ", maxSplits: 2).map(String.init)
        guard parts.count >= 2, let n = Int(parts[0]), n > acceptAnswered else { return }
        acceptAnswered = n
        let argument = parts.count > 2 ? parts[2] : ""
        switch parts[1] {
        case "state":
            acceptAnswer(n, acceptState())
        case "type":
            core.text(text: argument)
            key("Enter")
            acceptAnswer(n, "typed")
        case "expect":
            // The rows as they are now, not as last published.
            let rows = ViewJSON.decode(ScreenText.self, core.screenText())?.text ?? ""
            acceptAnswer(n, rows.contains(argument) ? "found" : "missing")
        case "measure":
            // From now until "measured", asked for once what is measured
            // has been set going: a stall it starts with is in it.
            longestTickGap = 0
            lastTickAt = 0
            measuringTicks = true
            // A link at rest ticks for nobody: the gauge needs it going.
            wakeLink()
            acceptAnswer(n, "measuring")
        case "measured":
            // The longest the main thread went without a frame callback.
            measuringTicks = false
            acceptAnswer(n, String(format: "longest_gap_ms=%.0f", longestTickGap * 1000))
        default:
            acceptAnswer(n, "unknown")
        }
    }

    /// One line the driver parses: the connection, and the tabs and bars
    /// the phone shows.
    private func acceptState() -> String {
        refreshViews()
        let current = tabs?.tabs.first(where: { $0.current })
        let pane = connection.hasPrefix("pane ") ? connection.dropFirst(5).prefix(while: { $0.isNumber }) : "-"
        // The status line stays on the pane through the App's own redials;
        // its sticky remark is what says the link is down.
        let connected = connection.hasPrefix("pane ") && !reconnecting ? "yes" : "no"
        return "connected=\(connected) pane=\(pane) tab=\(current.map { String($0.tab) } ?? "-")"
            + " tabs=\(tabs?.tabs.count ?? 0) panes=\(current?.panes.count ?? 0) bars=\(navs.count)"
    }

    private func acceptAnswer(_ n: Int, _ detail: String) {
        let line = "ACCEPT ack \(n) \(detail)"
        print(line)
        log(line)
    }

    /// The App's chrome over the FFI: split, list the tabs and bars, open a
    /// new tab, switch, close, and the sidebar rows.
    private func scheduleUiTest() {
        let q = DispatchQueue.main
        q.asyncAfter(deadline: .now() + 1.5) { self.connectProbe() }
        q.asyncAfter(deadline: .now() + 5.0) {
            self.refreshViews()
            self.check("tabs view", (self.tabs?.tabs.count ?? 0) >= 1, "\(self.tabs?.tabs.count ?? -1) tabs")
            // A lone pane has no bar on the phone (Platform::bare_lone_pane).
            self.check("lone pane, no bar", self.navs.isEmpty, "\(self.navs.count) bars")
            self.chromeClick("split-right")
        }
        q.asyncAfter(deadline: .now() + 7.0) {
            self.refreshViews()
            self.check("split drew two bars", self.navs.count == 2, "\(self.navs.count) bars")
            let sidebar = self.sidebar?.rows.count ?? 0
            self.check("sidebar rows", sidebar >= 1, "\(sidebar) rows")
            self.chromeClick("new-tab")
        }
        q.asyncAfter(deadline: .now() + 9.0) {
            self.refreshViews()
            let n = self.tabs?.tabs.count ?? 0
            self.check("new tab listed", n >= 2, "\(n) tabs")
            if let first = self.tabs?.tabs.first(where: { !$0.current }) {
                self.chromeClick("pane", pane: first.target)
            }
        }
        q.asyncAfter(deadline: .now() + 11.0) {
            self.refreshViews()
            self.check("switched tab", self.tabs?.tabs.first(where: { $0.current })?.panes.count == 2, "current has \(self.tabs?.tabs.first(where: { $0.current })?.panes.count ?? -1) panes")
            let menu = self.contextMenu("pane", id: String(self.tabs?.tabs.first(where: { $0.current })?.target ?? 0))
            self.check("pane menu", !menu.isEmpty, "\(menu.count) items")
            self.chromeClick("close")
        }
        q.asyncAfter(deadline: .now() + 13.0) {
            self.refreshViews()
            self.check("closed pane, bars gone", self.navs.isEmpty, "\(self.navs.count) bars")
            // The size from Settings reaches the open terminal, not only the next one.
            let size = self.settings.fontSize
            self.settings.fontSize = size + 2
            q.asyncAfter(deadline: .now() + 1.5) {
                self.refreshViews()
                self.check("text size is live", abs(self.fontPt - (size + 2)) < 0.01, "\(size) -> \(self.fontPt) pt")
                self.settings.fontSize = size
                self.report("ui done")
                print("UITEST DONE")
            }
        }
    }

    /// The native selection's document against the core: the rows read
    /// back, a programmatic selection lands in the core and copies.
    /// Threads on a bare server: a project, two threads in it, and the
    /// two-level strip that follows.
    private func scheduleTreeTest() {
        let q = DispatchQueue.main
        var project: String?
        q.asyncAfter(deadline: .now() + 1.5) { self.connectProbe() }
        q.asyncAfter(deadline: .now() + 4.0) { self.sideClick("new-project") }
        q.asyncAfter(deadline: .now() + 4.6) { self.sideKey("Enter", value: "/tmp/ttp-srv") }
        q.asyncAfter(deadline: .now() + 6.5) {
            self.refreshViews()
            project = self.sidebar?.rows.compactMap { row -> String? in
                if case .project(let id, _, _, _, false) = row { return id }
                return nil
            }.first
            self.check("project made", project != nil, self.sidebar?.new_project_error ?? "\(self.sidebar?.rows.count ?? -1) rows")
            if let project { self.sideClick("new-thread", id: project) }
        }
        q.asyncAfter(deadline: .now() + 9.5) {
            self.refreshViews()
            let threads = self.threads?.threads ?? []
            self.check("thread listed", threads.count >= 1, "\(threads.count) threads")
            self.check("thread current with a tab", threads.contains { $0.current && !$0.tabs.isEmpty }, threads.map { "\($0.name):\($0.tabs.count):\($0.current)" }.joined(separator: " "))
            if let project { self.sideClick("new-thread", id: project) }
        }
        q.asyncAfter(deadline: .now() + 12.5) {
            self.refreshViews()
            let threads = self.threads?.threads ?? []
            self.check("second thread", threads.count >= 2, "\(threads.count) threads")
            self.check("one current", threads.filter(\.current).count == 1, threads.map { "\($0.name):\($0.current)" }.joined(separator: " "))
            self.report("tree done")
            print("TREETEST DONE")
        }
    }

    private func scheduleSelectionTest() {
        let q = DispatchQueue.main
        q.asyncAfter(deadline: .now() + 1.5) { self.connectProbe() }
        q.asyncAfter(deadline: .now() + 4.0) { self.core.text(text: "echo SELECT-ME-PLEASE\n") }
        q.asyncAfter(deadline: .now() + 6.0) {
            guard let input = self.inputView else { return self.check("input view", false, "none") }
            let json = self.core.screenText()
            guard let screen = ViewJSON.decode(ScreenText.self, json) else {
                return self.check("screen text", false, String(json.prefix(80)))
            }
            let stride = screen.cols + 1
            self.check("screen text shape", screen.text.utf16.count == screen.rows * stride, "\(screen.rows)x\(screen.cols), \(screen.text.utf16.count) units")
            // The echoed line: the last row holding it.
            let rows = screen.text.split(separator: "\n", omittingEmptySubsequences: false).map(String.init)
            guard let row = rows.lastIndex(where: { $0.contains("SELECT-ME-PLEASE") }),
                  let col = rows[row].range(of: "SELECT-ME-PLEASE")?.lowerBound.utf16Offset(in: rows[row]) else {
                return self.check("echoed line on screen", false, "not found")
            }
            let a = row * stride + col
            let range = TerminalInputView.Range(a, a + "SELECT-ME-PLEASE".count)
            input.selectedTextRange = range
            let read = input.text(in: range) ?? ""
            self.check("document text", read == "SELECT-ME-PLEASE", read)
            q.asyncAfter(deadline: .now() + 0.5) {
                let copied = input.selectedText() ?? ""
                self.check("shell selection", copied == "SELECT-ME-PLEASE", copied)
                let back = input.selectedTextRange as? TerminalInputView.Range
                self.check("selection read back", back?.a == a && back?.b == a + "SELECT-ME-PLEASE".count, "\(back?.a ?? -1)..\(back?.b ?? -1)")
                let rects = input.selectionRects(for: range)
                self.check("selection rects", rects.count == 1 && rects[0].rect.width > 0, "\(rects.count) rects, first \(rects.first?.rect ?? .zero)")
                let under = input.closestPosition(to: CGPoint(x: rects.first?.rect.midX ?? 0, y: rects.first?.rect.midY ?? 0)) as? TerminalInputView.Pos
                self.check("point to cell", under.map { $0.i >= a && $0.i < a + 16 } ?? false, "\(under?.i ?? -1)")
                // Wide characters and one outside the BMP: the rows carry
                // stand-ins for both, the copy must not.
                self.core.text(text: "echo 复制中文😀OK\n")
            }
            q.asyncAfter(deadline: .now() + 2.5) {
                let wide = "复制中文😀OK"
                guard let screen = ViewJSON.decode(ScreenText.self, self.core.screenText()) else {
                    return self.check("wide line on screen", false, "no rows")
                }
                let rows = screen.text.split(separator: "\n", omittingEmptySubsequences: false).map(String.init)
                guard let row = rows.lastIndex(where: { $0.contains("OK") && $0.contains("文") && !$0.contains("echo") }) else {
                    return self.check("wide line on screen", false, "not found")
                }
                let start = row * (screen.cols + 1)
                input.selectedTextRange = TerminalInputView.Range(start, start + screen.cols)
                let copied = input.selectedText() ?? ""
                self.check("copy keeps wide text", copied == wide, copied)
                self.report("selection done")
                print("SELTEST DONE")
            }
        }
    }

    /// `--uploadtest`: a small picture left on the probe's host twice
    /// under one name, as the attach button would leave it; the second
    /// keeps both. Each answer is on the console.
    private func scheduleUploadTest() {
        let q = DispatchQueue.main
        q.asyncAfter(deadline: .now() + 1.5) { self.connectProbe() }
        q.asyncAfter(deadline: .now() + 9.0) {
            let picture = UIGraphicsImageRenderer(size: CGSize(width: 8, height: 8)).image { context in
                UIColor.systemBlue.setFill()
                context.fill(CGRect(x: 0, y: 0, width: 8, height: 8))
            }
            guard let data = picture.pngData() else { return self.check("upload picture", false, "none made") }
            var ended: [Upload] = []
            self.onUploadEnded = { upload in
                ended.append(upload)
                if ended.count == 1 {
                    self.check("upload lands in the uploads folder", upload.path?.hasSuffix("/ThinkTerm_Uploads/Upload test.png") == true, upload.path ?? upload.error ?? "")
                    self.check("all of it went", upload.total > 0 && upload.sent == upload.total, "\(upload.sent) of \(upload.total)")
                    // One at a time, so which is first is not a race.
                    self.upload(name: "Upload test.png", data: data)
                } else if ended.count == 2 {
                    self.check("a second keeps both", upload.path?.hasSuffix("/ThinkTerm_Uploads/Upload test (1).png") == true, upload.path ?? upload.error ?? "")
                    // A larger one, given up as it sets off: nothing of it stays.
                    self.upload(name: "Given up.bin", data: Data(count: 8 << 20))
                    if let last = self.uploads.last { self.cancelUpload(last.id) }
                } else if ended.count == 3 {
                    self.check("one given up says so", upload.error == "cancelled", upload.error ?? upload.path ?? "")
                }
            }
            self.upload(name: "Upload test.png", data: data)
        }
    }

    private func scheduleImeTest() {
        let q = DispatchQueue.main
        var sent: Int { Int(inputsSent()) }
        var sizeBefore = ""
        q.asyncAfter(deadline: .now() + 1.5) { self.connectProbe() }
        q.asyncAfter(deadline: .now() + 5.0) {
            sizeBefore = self.sizeString()
            let asked = Date()
            self.focusKeyboard()
            // The first keyboard in a while is slow to come up: wait for the
            // resize, up to 5 s, rather than a fixed time.
            func look() {
                let after = self.sizeString()
                let waited = Date().timeIntervalSince(asked)
                if after != sizeBefore || waited > 5 {
                    self.check(
                        "keyboard resize", after != sizeBefore,
                        "\(sizeBefore) -> \(after) after \(String(format: "%.1f", waited)) s"
                    )
                } else {
                    q.asyncAfter(deadline: .now() + 0.1, execute: look)
                }
            }
            look()
        }
        q.asyncAfter(deadline: .now() + 7.0) {
            guard let v = self.inputView else { self.check("input view", false, "missing"); return }
            let base = sent
            v.setMarkedText("n", selectedRange: NSRange(location: 1, length: 0))
            v.setMarkedText("ni", selectedRange: NSRange(location: 2, length: 0))
            v.setMarkedText("nih", selectedRange: NSRange(location: 3, length: 0))
            q.asyncAfter(deadline: .now() + 0.3) {
                self.check("composing sends nothing", sent == base, "sent \(sent - base), composing=\(self.composingFlag())")
                v.insertText("你")
                v.unmarkText()
                q.asyncAfter(deadline: .now() + 0.3) {
                    self.check("candidate commits once", sent == base + 1, "sent \(sent - base)")
                    let b2 = sent
                    v.setMarkedText("hao", selectedRange: NSRange(location: 3, length: 0))
                    v.setMarkedText("", selectedRange: NSRange(location: 0, length: 0))
                    q.asyncAfter(deadline: .now() + 0.3) {
                        self.check("cancel sends nothing", sent == b2, "sent \(sent - b2)")
                        v.setMarkedText("hao", selectedRange: NSRange(location: 3, length: 0))
                        v.deleteBackward()
                        v.setMarkedText("ha", selectedRange: NSRange(location: 2, length: 0))
                        q.asyncAfter(deadline: .now() + 0.3) {
                            self.check("backspace edits composition only", sent == b2 && v.marked == "ha", "sent \(sent - b2), marked=\(v.marked ?? "nil")")
                            v.unmarkText()
                            q.asyncAfter(deadline: .now() + 0.3) {
                                self.check("leftover composition commits once", sent == b2 + 1, "sent \(sent - b2)")
                                // A dictation hears its last word again: four
                                // backspaces and the better word.
                                let b3 = sent
                                v.insertText("helo wrld")
                                if let end = v.position(from: v.endOfDocument, offset: 0),
                                   let start = v.position(from: end, offset: -4),
                                   let word = v.textRange(from: start, to: end) {
                                    let read = v.text(in: word) ?? ""
                                    v.replace(word, withText: "world")
                                    q.asyncAfter(deadline: .now() + 0.3) {
                                        self.check(
                                            "a replaced word is typed over", read == "wrld" && sent == b3 + 6 && v.typed.hasSuffix("helo world"),
                                            "read \(read), sent \(sent - b3), typed \(v.typed)"
                                        )
                                        let b4 = sent
                                        v.insertText(" ")
                                        v.deleteBackward()
                                        v.insertText(". ")
                                        self.key("ArrowLeft")
                                        v.replace(word, withText: "x")
                                        q.asyncAfter(deadline: .now() + 0.3) {
                                            self.check(
                                                "two spaces stay, and a moved line is typed into", sent == b4 + 5 && v.typed.hasSuffix("helo world  x"),
                                                "sent \(sent - b4), typed \(v.typed)"
                                            )
                                            self.key("Enter")
                                        }
                                    }
                                }
                                q.asyncAfter(deadline: .now() + 2.5) {
                                    self.report("ime done")
                                    print("IMETEST DONE")
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    private func check(_ name: String, _ ok: Bool, _ detail: String) {
        let line = "UICHECK \(ok ? "PASS" : "FAIL") \(name): \(detail)"
        print(line)
        log(line)
    }

    private func statsField(_ key: String) -> String {
        let s = core.stats()
        guard let r = s.range(of: "\"\(key)\":") else { return "" }
        let rest = s[r.upperBound...]
        return String(rest.prefix { $0 != "," && $0 != "}" }).replacingOccurrences(of: "\"", with: "")
    }

    private func inputsSent() -> Int64 { Int64(statsField("inputs_sent")) ?? -1 }
    private func composingFlag() -> String { statsField("composing") }
    private func sizeString() -> String { statsField("size") }
    #endif
}

#if DEBUG
/// The Mac user the probe's key belongs to: the `user@host` comment
/// ssh-keygen wrote at the end of the public key. The simulator gives an
/// app neither USER nor a real NSUserName.
func probeUserName() -> String {
    if let pub = try? String(contentsOfFile: "/tmp/ttp-ssh/userkey.pub", encoding: .utf8),
       let comment = pub.split(separator: " ").dropFirst(2).first,
       let user = comment.split(separator: "@").first, !user.isEmpty {
        return String(user)
    }
    let env = ProcessInfo.processInfo.environment["USER"] ?? ""
    return env.isEmpty ? NSUserName() : env
}
#endif

/// UniFFI hands the callback object to the core thread; it forwards to the
/// model without touching anything that needs the main thread.
private final class NotifySink: Notify, @unchecked Sendable {
    weak var model: TerminalModel?
    private let screenReader = DispatchQueue(label: "thinkterm.screens", qos: .userInitiated)
    func onFrameNeeded() { model?.coreWantsFrame() }
    func onStatus(status: String) { model?.coreStatus(status) }
    func onLog(line: String) { model?.coreLogged(line) }
    func onTitle(title: String) { model?.coreTitle(title) }
    /// On screen the display link reads the views' count each frame; off
    /// it, a list showing the host's threads is told.
    func onChange() { model?.coreChanged() }
    func onClipboard(text: String) {
        DispatchQueue.main.async {
            UIPasteboard.general.string = text
            self.model?.showCopied()
        }
    }
    /// The App asks for the input's focus after a press, a finished
    /// rename and every answered request. On a phone that would raise
    /// the keyboard over half the screen at each of them, so the ask is
    /// honoured only while the keyboard is already up -- and then the
    /// input is already first responder. A tap raises it on its own.
    func onFocusInput() {}
    func onImeAnchor(left: Double, top: Double, width: Double, height: Double) {
        DispatchQueue.main.async {
            self.model?.cursorRect = CGRect(x: left, y: top, width: width, height: height)
        }
    }
    func onHostKeyRequired(requestId: String, fingerprint: String, jump: Bool) { model?.coreHostKeyRequired(requestId, fingerprint, jump) }
    func onPublished(key: String, value: String) { model?.corePublished(key, value) }
    func onBell() { model?.coreBell() }
    func onPreview(pane: UInt32, rows: String) {
        // Only whole screens are asked for here (`requestScreen`), read off
        // the core's thread, in the order they come.
        screenReader.async {
            guard let screen = ViewJSON.decode(TabPreview.self, rows) else { return }
            DispatchQueue.main.async { self.model?.coreScreen(Int(pane), screen) }
        }
    }
}

/// Paints the graphemes the bundled faces lack with CoreText, which falls
/// through the system's cascade list (PingFang for Han, Apple Color Emoji
/// for emoji) on its own. Called on the core thread.
///
/// A character that is text unless asked to be a picture -- the record
/// mark a program heads its lines with, a check, a warning sign -- is
/// drawn as text: the cascade hands such a one to the emoji font whenever
/// Menlo lacks it, and a terminal would show a coloured button where the
/// desktop shows a plain mark.
final class CoreTextPainter: GlyphPainter, @unchecked Sendable {
    /// Whether `text` asks for a picture: it says so with the variation
    /// selector, or is a character that is one by default.
    private static func asksForPicture(_ text: String) -> Bool {
        let scalars = text.unicodeScalars
        if scalars.contains(where: { $0.value == 0xFE0F }) { return true }
        if scalars.contains(where: { $0.value == 0xFE0E }) { return false }
        // A sequence -- a flag, a family, a skin tone -- is a picture.
        return scalars.count > 1 || scalars.first?.properties.isEmojiPresentation == true
    }

    /// The face to draw `text` in: the cascade's, unless that is the emoji
    /// font for text that did not ask for a picture and a plain face has
    /// it.
    private static func face(for text: String, px: Double) -> CTFont {
        let base = CTFontCreateWithName("Menlo" as CFString, CGFloat(px), nil)
        let units = Array(text.utf16)
        let cascade = CTFontCreateForString(base, text as CFString, CFRange(location: 0, length: units.count))
        guard (CTFontCopyPostScriptName(cascade) as String).hasPrefix("AppleColorEmoji"), !asksForPicture(text) else {
            return cascade
        }
        for name in TerminalFaces.plainFaces {
            let plain = CTFontCreateWithName(name as CFString, CGFloat(px), nil)
            var glyphs = [CGGlyph](repeating: 0, count: units.count)
            // A name the system has not got comes back as another face:
            // only the one asked for counts.
            if (CTFontCopyPostScriptName(plain) as String) == name,
               CTFontGetGlyphsForCharacters(plain, units, &glyphs, units.count) {
                return plain
            }
        }
        return cascade
    }

    func paint(text: String, px: Double, width: UInt32, height: UInt32, penX: Double, baseline: Double, red: UInt8, green: UInt8, blue: UInt8) -> Data {
        let w = Int(width), h = Int(height)
        guard w > 0, h > 0 else { return Data() }
        let colorSpace = CGColorSpaceCreateDeviceRGB()
        guard let ctx = CGContext(
            data: nil, width: w, height: h, bitsPerComponent: 8, bytesPerRow: w * 4,
            space: colorSpace, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
        ) else { return Data() }
        ctx.clear(CGRect(x: 0, y: 0, width: w, height: h))
        // Menlo's own cascade list does not reach Apple Color Emoji on iOS;
        // asking CoreText for the font that can draw this string does.
        let font = Self.face(for: text, px: px)
        let color = CGColor(red: CGFloat(red) / 255, green: CGFloat(green) / 255, blue: CGFloat(blue) / 255, alpha: 1)
        let attrs: [NSAttributedString.Key: Any] = [
            NSAttributedString.Key(kCTFontAttributeName as String): font,
            NSAttributedString.Key(kCTForegroundColorAttributeName as String): color,
        ]
        let line = CTLineCreateWithAttributedString(NSAttributedString(string: text, attributes: attrs))
        // CoreGraphics draws with the origin at the bottom-left, but a
        // bitmap context's first row in memory is the top of the image, so
        // the baseline is flipped for drawing and the rows are read as-is.
        ctx.textMatrix = .identity
        ctx.textPosition = CGPoint(x: penX, y: Double(h) - baseline)
        CTLineDraw(line, ctx)
        guard let base = ctx.data else { return Data() }
        let src = base.assumingMemoryBound(to: UInt8.self)
        var out = [UInt8](repeating: 0, count: w * h * 4)
        for row in 0..<h {
            let srcRow = row * w * 4
            let dstRow = row * w * 4
            for x in 0..<w {
                let s = srcRow + x * 4, d = dstRow + x * 4
                let a = src[s + 3]
                if a == 0 { continue }
                let scale = 255.0 / Double(a)
                out[d] = UInt8(min(255.0, Double(src[s]) * scale))
                out[d + 1] = UInt8(min(255.0, Double(src[s + 1]) * scale))
                out[d + 2] = UInt8(min(255.0, Double(src[s + 2]) * scale))
                out[d + 3] = a
            }
        }
        return Data(out)
    }
}

final class AtomicFlag: @unchecked Sendable {
    private let lock = NSLock()
    private var value = false
    func set() { lock.lock(); value = true; lock.unlock() }
    func take() -> Bool { lock.lock(); defer { lock.unlock() }; let v = value; value = false; return v }
    func get() -> Bool { lock.lock(); defer { lock.unlock() }; return value }
}
