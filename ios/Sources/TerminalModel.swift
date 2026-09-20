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
    /// The last rows of panes the overview asked about, by pane.
    @Published var previews: [Int: [PreviewRow]] = [:]
    @Published var sidebar: SidebarView?
    @Published var tree: TreeView?
    @Published var navs: [NavView] = []
    @Published var status: StatusView?
    @Published var title = ""
    @Published var connection = ""
    /// The key panel is up in the keyboard's place.
    @Published var panelOpen = ProcessInfo.processInfo.arguments.contains("--keypanel")
    /// A thread this phone was on last time it was in this host, put
    /// back once the threads are listed after a connect.
    private var restoredThread = false
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
    /// The App says its connection is down and it is redialing.
    @Published private(set) var reconnecting = false
    private var toastTimer: Timer?
    /// The terminal's background, as the App paints it: the chrome around
    /// the terminal takes the same colour.
    @Published var background = Color(white: 0.11)
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
    /// The log view is open: the stats are worth refreshing.
    var wantsStats = false
    /// The preferences, followed as they change.
    let settings = AppSettings.shared
    var smoothScroll: Bool { settings.smoothScroll }
    private var subscriptions: [AnyCancellable] = []
    /// The App's cell height in points, from its layout view.
    private(set) var cellHeight: Double = 20

    let core: Core
    /// The host this screen shows; edited in place from the failure card.
    @Published var host: Host?
    weak var store: HostStore?
    weak var inputView: TerminalInputView?
    var cursorRect = CGRect(x: 8, y: 8, width: 2, height: 20)

    private let frameNeeded = AtomicFlag()
    private let changePending = AtomicFlag()
    private var displayLink: CADisplayLink?
    private var statsTimer: Timer?
    private var logLines: [String] = []
    private var hostKeyToRemember: String?

    weak var layer: CAMetalLayer?
    private(set) var generation: UInt64 = 0
    private var wantsSurface = false
    private var lastSize: (Int, Int, Double) = (0, 0, 0)

    // The probe's ssh setup, for the automated flows: a throwaway key and
    // a wrapper that points the remote proxy at an isolated HOME.
    private var probeKeyPath = "/tmp/ttp-ssh/userkey"
    private var probeRemoteCommand = "/tmp/ttp-ssh/thinkterm-remote cli --prefer-mux proxy"
    private var probeUser = probeUserName()

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
        if let u = arg("--user") { probeUser = u }
        if let k = arg("--key") { probeKeyPath = k }
        if let c = arg("--remote-command") { probeRemoteCommand = c }

        let link = CADisplayLink(target: self, selector: #selector(tick))
        link.add(to: .main, forMode: .common)
        displayLink = link
        // Every published change redraws the screen (and closes an open
        // menu): the stats tick only while the log shows them.
        statsTimer = Timer.scheduledTimer(withTimeInterval: 0.5, repeats: true) { [weak self] _ in
            guard let self, self.wantsStats else { return }
            let stats = self.core.stats()
            if stats != self.stats { self.stats = stats }
        }
        if let name = arg("--scheme") { settings.schemeName = name }
        applyScrollMode()
        applyScheme()
        settings.$schemeName.dropFirst().sink { [weak self] _ in self?.applyScheme() }.store(in: &subscriptions)
        settings.$smoothScroll.dropFirst().sink { [weak self] _ in self?.applyScrollMode() }.store(in: &subscriptions)
        applyTerminalPrefs()
        settings.$cursorStyle.dropFirst().sink { [weak self] _ in self?.applyTerminalPrefs() }.store(in: &subscriptions)
        settings.$cursorBlink.dropFirst().sink { [weak self] _ in self?.applyTerminalPrefs() }.store(in: &subscriptions)
        settings.$contrast.dropFirst().sink { [weak self] _ in self?.applyTerminalPrefs() }.store(in: &subscriptions)
        settings.$resizeMode.dropFirst().sink { [weak self] _ in self?.applyTerminalPrefs() }.store(in: &subscriptions)
        settings.$autoReconnect.dropFirst().sink { [weak self] _ in self?.applyTerminalPrefs() }.store(in: &subscriptions)
        settings.$paneBars.dropFirst().sink { [weak self] _ in self?.applyTerminalPrefs() }.store(in: &subscriptions)
        settings.$language.dropFirst().sink { [weak self] _ in self?.applyTerminalPrefs() }.store(in: &subscriptions)
        // A face is shaped at connect time: the connection is made again.
        settings.$fontFamily.dropFirst().removeDuplicates().sink { [weak self] _ in
            guard let self, self.connection.hasPrefix("pane ") else { return }
            self.disconnect()
            self.connect()
        }.store(in: &subscriptions)
        settings.$devMode.sink { [weak self] on in self?.wantsStats = on || (self?.wantsStats ?? false) }.store(in: &subscriptions)
        if args.contains("--autotest") { scheduleAutotest() }
        if args.contains("--autoconnect") { scheduleAutoconnect() }
        if args.contains("--imetest") { scheduleImeTest() }
        if args.contains("--seltest") { scheduleSelectionTest() }
        if args.contains("--treetest") { scheduleTreeTest() }
        if args.contains("--uitest") { scheduleUiTest() }
    }

    /// The screen went away but the connection stays: the frame loop
    /// rests until the screen is back. The surface goes with the view.
    func leaveScreen() { displayLink?.isPaused = true }
    func enterScreen() { displayLink?.isPaused = false }

    func shutdown() {
        displayLink?.invalidate()
        statsTimer?.invalidate()
        detach()
        core.disconnect()
        core.shutdown()
    }

    // MARK: connection

    /// The chosen face first, the symbols fallback after it.
    var fontPaths: [String] {
        let face = settings.fontFamily == "Fira Code" ? "FiraCode-Regular" : "JetBrainsMono-Regular"
        return [face, "SymbolsNerdFontMono-Regular"].compactMap {
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

    /// Connect to the model's host with the secret from the Keychain.
    func connect() {
        restoredThread = false
        guard let host else {
            connectProbe()
            return
        }
        let account = host.id.uuidString
        let secret = Keychain.load(account: account) ?? ""
        let passphrase = Keychain.load(account: account + ".passphrase")
        log("shell: \(host.auth == .password ? "password" : "key text") \(secret.count) chars from the Keychain" + (passphrase == nil ? "" : ", with a passphrase"))
        connect(
            hostname: host.hostname, port: host.port, user: host.user,
            authKind: host.auth == .password ? "password" : "key", secret: secret,
            passphrase: passphrase, knownHost: host.knownHost, remoteCommand: host.remoteCommand
        )
    }

    /// The probe's connection: the private sshd on this Mac.
    func connectProbe() {
        connect(
            hostname: "127.0.0.1", port: 2299, user: probeUser, authKind: "key-path",
            secret: probeKeyPath, passphrase: nil, knownHost: nil, remoteCommand: probeRemoteCommand
        )
    }

    private func connect(hostname: String, port: Int, user: String, authKind: String, secret: String, passphrase: String?, knownHost: String?, remoteCommand: String) {
        let paths = fontPaths
        guard paths.count == 2 else {
            log("shell: fonts missing from the bundle (\(paths))")
            return
        }
        core.connect(
            host: hostname, port: UInt16(clamping: port), user: user,
            authKind: authKind, secret: secret, passphrase: passphrase, knownHost: knownHost,
            remoteCommand: remoteCommand, deviceId: Self.deviceId, keepaliveSecs: UInt32(max(settings.keepAliveSeconds, 0)),
            fontPaths: paths, sizePt: settings.fontSize,
            painter: CoreTextPainter()
        )
    }

    func disconnect() { core.disconnect() }

    // MARK: input

    func key(_ name: String, ctrl: Bool = false, alt: Bool = false, shift: Bool = false) {
        // A sticky modifier is spent by the next key whether or not that
        // key brought the modifier itself, so it never lingers past it.
        let stickyCtrl = takeSticky(\.ctrlSticky)
        let stickyAlt = takeSticky(\.altSticky)
        core.key(name: name, ctrl: ctrl || stickyCtrl, alt: alt || stickyAlt, shift: shift)
    }

    /// Text from the keyboard. A sticky Ctrl or Alt turns a single
    /// character into a chord.
    func text(_ text: String) {
        if (ctrlSticky || altSticky), text.count == 1 {
            key(text)
            return
        }
        core.text(text: text)
    }

    func pasteFromClipboard() {
        if let text = UIPasteboard.general.string, !text.isEmpty {
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
        if reconnecting != sticky { reconnecting = sticky }
        guard let toast, !toast.text.isEmpty else {
            if toastText != nil { toastText = nil }
            return
        }
        toastText = toast.text
        if !sticky {
            toastTimer = Timer.scheduledTimer(withTimeInterval: 4, repeats: false) { [weak self] _ in
                guard let self, self.toastText == toast.text else { return }
                self.toastText = nil
            }
        }
    }

    private func applyScheme() {
        let schemeName = settings.schemeName
        if schemeName == Schemes.followDesktop || Schemes.json(named: schemeName) == nil {
            core.setSetting(key: "terminal-scheme", value: "\"desktop\"")
            core.setPalette(scheme: nil)
        } else {
            core.setSetting(key: "terminal-scheme", value: "\"" + schemeName.replacingOccurrences(of: "\"", with: "\\\"") + "\"")
            core.setPalette(scheme: Schemes.json(named: schemeName))
        }
    }

    fileprivate func corePublished(_ key: String, _ value: String) {
        guard key == "bg", let color = Color(hex: value) else { return }
        DispatchQueue.main.async {
            if self.background != color { self.background = color }
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
        core.setSetting(key: "auto-reconnect", value: settings.autoReconnect ? "true" : "false")
        core.setSetting(key: "pane-bars", value: settings.paneBars ? "true" : "false")
        // The core's own menus and messages follow the app's language.
        core.setSetting(key: "language", value: "\"\(L10n.currentTag)\"")
    }

    /// The tab `by` places along the strip from the current one, shown.
    func switchTab(by: Int) {
        guard let tabs = tabs?.tabs, !tabs.isEmpty,
              let at = tabs.firstIndex(where: { $0.current }) else { return }
        let next = at + by
        guard tabs.indices.contains(next) else { return }
        chromeClick("pane", pane: tabs[next].target)
    }

    /// The last rows of a pane, for a thumbnail; `previews` fills in.
    func requestPreview(pane: Int, rows: Int = 8) {
        core.requestPreview(pane: UInt32(pane), rows: UInt32(rows))
    }

    /// A pane rang its bell: a buzz, if the setting says so.
    fileprivate func coreBell() {
        DispatchQueue.main.async {
            guard self.settings.bell else { return }
            UINotificationFeedbackGenerator().notificationOccurred(.warning)
        }
    }

    func stepFont(_ by: Double) { core.stepFont(by: by) }

    // MARK: the App's chrome

    func chromeClick(_ action: String, pane: Int? = nil, tab: Int? = nil) {
        core.chromeClick(action: action, pane: pane.map { UInt32($0) }, tab: tab.map { UInt32($0) })
    }

    func sideClick(_ kind: String, id: String? = nil, flag: Bool? = nil) {
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

    /// Pull every view the screen shows. Called on the main thread after
    /// the core said something changed; a burst of changes is one pull.
    func refreshViews() {
        inputView?.screenChanged()
        // Assigned only on a change: a published write redraws the whole
        // screen, and the core reports changes as often as output comes.
        let tabs = ViewJSON.decode(TabsView.self, core.view(name: "tabs"))
        if tabs != self.tabs { self.tabs = tabs }
        let sidebar = ViewJSON.decode(SidebarView.self, core.view(name: "sidebar"))
        if sidebar != self.sidebar { self.sidebar = sidebar }
        let threads = ViewJSON.decode(ThreadsView.self, core.view(name: "threads"))
        if threads != self.threads { self.threads = threads }
        rememberThread(threads)
        let tree = ViewJSON.decode(TreeView.self, core.view(name: "tree"))
        if tree != self.tree { self.tree = tree }
        let navs = ViewJSON.decode([NavView].self, core.view(name: "navs")) ?? []
        if navs != self.navs { self.navs = navs }
        let status = ViewJSON.decode(StatusView.self, core.view(name: "status"))
        if status != self.status {
            self.status = status
            showToast(status?.toast)
        }
        let layout = core.view(name: "layout")
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
    private var pausedForBackground = false

    func detachForBackground() {
        detach()
        report("background")
        if settings.keepSessionInBackground {
            // iOS grants about half a minute; the socket lives that long.
            backgroundTask = UIApplication.shared.beginBackgroundTask(withName: "thinkterm-connection") { [weak self] in
                self?.endBackgroundTask()
            }
        } else if !connection.hasPrefix("idle"), !connection.hasPrefix("disconnected"), !connection.hasPrefix("failed") {
            // Attached or still on the way: torn down for the background,
            // and dialled again on the way back -- whatever the status
            // reads by then.
            pausedForBackground = true
            disconnect()
        }
    }

    private func endBackgroundTask() {
        if backgroundTask != .invalid {
            UIApplication.shared.endBackgroundTask(backgroundTask)
            backgroundTask = .invalid
        }
    }

    func reattachAfterBackground() {
        endBackgroundTask()
        if pausedForBackground {
            pausedForBackground = false
            connect()
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

    @objc private func tick() {
        if frameNeeded.take() {
            core.render()
        }
        if changePending.take() {
            refreshViews()
        }
    }

    func requestOneFrame() { frameNeeded.set() }

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

    fileprivate func coreWantsFrame() { frameNeeded.set() }
    fileprivate func coreChanged() { changePending.set() }

    fileprivate func coreStatus(_ status: String) {
        DispatchQueue.main.async {
            self.connection = status
            self.log("status: " + status)
            if status.hasPrefix("pane "), let host = self.host {
                self.store?.touchConnected(host.id)
            }
            if status.hasPrefix("connecting") || status.contains("reconnect") || status.hasPrefix("disconnected") {
                self.inputView?.dropComposition()
            }
            self.changePending.set()
        }
    }

    fileprivate func coreTitle(_ title: String) {
        DispatchQueue.main.async {
            if self.title != title { self.title = title }
        }
    }

    fileprivate func coreHostKey(_ fingerprint: String) {
        DispatchQueue.main.async {
            self.log("host key " + fingerprint)
            if let host = self.host {
                self.store?.rememberHostKey(fingerprint, for: host.id)
            }
        }
    }

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

    /// The App's chrome over the FFI: split, list the tabs and bars, open a
    /// new tab, switch, close, and the sidebar rows.
    private func scheduleUiTest() {
        let q = DispatchQueue.main
        q.asyncAfter(deadline: .now() + 1.5) { self.connectProbe() }
        q.asyncAfter(deadline: .now() + 5.0) {
            self.refreshViews()
            self.check("tabs view", (self.tabs?.tabs.count ?? 0) >= 1, "\(self.tabs?.tabs.count ?? -1) tabs")
            self.check("nav bars", self.navs.count == 1, "\(self.navs.count) bars")
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
            self.check("closed pane", self.navs.count == 1, "\(self.navs.count) bars")
            self.report("ui done")
            print("UITEST DONE")
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
                self.report("selection done")
                print("SELTEST DONE")
            }
        }
    }

    private func scheduleImeTest() {
        let q = DispatchQueue.main
        var sent: Int { Int(inputsSent()) }
        var sizeBefore = ""
        q.asyncAfter(deadline: .now() + 1.5) { self.connectProbe() }
        q.asyncAfter(deadline: .now() + 5.0) {
            sizeBefore = self.sizeString()
            self.focusKeyboard()
        }
        q.asyncAfter(deadline: .now() + 6.5) {
            let after = self.sizeString()
            self.check("keyboard resize", after != sizeBefore, "\(sizeBefore) -> \(after)")
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
                                self.key("Enter")
                                q.asyncAfter(deadline: .now() + 1.5) {
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
}

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

/// UniFFI hands the callback object to the core thread; it forwards to the
/// model without touching anything that needs the main thread.
private final class NotifySink: Notify, @unchecked Sendable {
    weak var model: TerminalModel?
    func onFrameNeeded() { model?.coreWantsFrame() }
    func onStatus(status: String) { model?.coreStatus(status) }
    func onLog(line: String) { model?.coreLogged(line) }
    func onTitle(title: String) { model?.coreTitle(title) }
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
    func onHostKey(fingerprint: String) { model?.coreHostKey(fingerprint) }
    func onPublished(key: String, value: String) { model?.corePublished(key, value) }
    func onBell() { model?.coreBell() }
    func onPreview(pane: UInt32, rows: String) {
        let decoded = ViewJSON.decode([PreviewRow].self, rows) ?? []
        DispatchQueue.main.async { self.model?.previews[Int(pane)] = decoded }
    }
}

/// Paints the graphemes the bundled faces lack with CoreText, which falls
/// through the system's cascade list (PingFang for Han, Apple Color Emoji
/// for emoji) on its own. Called on the core thread.
final class CoreTextPainter: GlyphPainter, @unchecked Sendable {
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
        let base = CTFontCreateWithName("Menlo" as CFString, CGFloat(px), nil)
        let font = CTFontCreateForString(base, text as CFString, CFRange(location: 0, length: (text as NSString).length))
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
}
