import Foundation
import UIKit
import QuartzCore
import Combine
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
    @Published var sidebar: SidebarView?
    @Published var navs: [NavView] = []
    @Published var status: StatusView?
    @Published var title = ""
    @Published var connection = ""
    @Published var attached = false
    @Published var composing: String?
    @Published var ctrlSticky = false
    @Published var altSticky = false
    @Published var logText = ""
    @Published var stats = ""
    @Published var inset: CGFloat = 0
    @Published var animating = false
    /// Smooth (by the pixel, with inertia) or stepped (whole rows).
    @Published var smoothScroll: Bool = UserDefaults.standard.object(forKey: "scroll.smooth") as? Bool ?? true {
        didSet {
            UserDefaults.standard.set(smoothScroll, forKey: "scroll.smooth")
            applyScrollMode()
        }
    }
    /// The App's cell height in points, from its layout view.
    private(set) var cellHeight: Double = 20

    let core: Core
    let host: Host?
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
        statsTimer = Timer.scheduledTimer(withTimeInterval: 0.5, repeats: true) { [weak self] _ in
            guard let self else { return }
            self.stats = self.core.stats()
        }
        applyScrollMode()
        if args.contains("--autotest") { scheduleAutotest() }
        if args.contains("--autoconnect") { scheduleAutoconnect() }
        if args.contains("--imetest") { scheduleImeTest() }
        if args.contains("--uitest") { scheduleUiTest() }
    }

    func shutdown() {
        displayLink?.invalidate()
        statsTimer?.invalidate()
        detach()
        core.disconnect()
        core.shutdown()
    }

    // MARK: connection

    var fontPaths: [String] {
        ["JetBrainsMono-Regular", "SymbolsNerdFontMono-Regular"].compactMap {
            Bundle.main.path(forResource: $0, ofType: "ttf")
        }
    }

    /// Connect to the model's host with the secret from the Keychain.
    func connect() {
        guard let host else {
            connectProbe()
            return
        }
        let account = host.id.uuidString
        let secret = Keychain.load(account: account) ?? ""
        let passphrase = Keychain.load(account: account + ".passphrase")
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
            remoteCommand: remoteCommand, fontPaths: paths, sizePt: 11.0,
            painter: CoreTextPainter()
        )
    }

    func disconnect() { core.disconnect() }

    // MARK: input

    func key(_ name: String, ctrl: Bool = false, alt: Bool = false, shift: Bool = false) {
        core.key(name: name, ctrl: ctrl || takeSticky(\.ctrlSticky), alt: alt || takeSticky(\.altSticky), shift: shift)
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

    private func applyScrollMode() {
        core.setSetting(key: "scroll-mode", value: smoothScroll ? "\"smooth\"" : "\"stepped\"")
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
        tabs = ViewJSON.decode(TabsView.self, core.view(name: "tabs"))
        sidebar = ViewJSON.decode(SidebarView.self, core.view(name: "sidebar"))
        navs = ViewJSON.decode([NavView].self, core.view(name: "navs")) ?? []
        status = ViewJSON.decode(StatusView.self, core.view(name: "status"))
        let layout = core.view(name: "layout")
        if let r = layout.range(of: "\"cell\":["),
           let end = layout[r.upperBound...].firstIndex(of: "]") {
            let parts = layout[r.upperBound..<end].split(separator: ",")
            if parts.count == 2, let h = Double(parts[1]), h > 0 { cellHeight = h }
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

    func detachForBackground() {
        detach()
        report("background")
    }

    func reattachAfterBackground() {
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
            if status.hasPrefix("connecting") || status.contains("reconnect") || status.hasPrefix("disconnected") {
                self.inputView?.dropComposition()
            }
            self.changePending.set()
        }
    }

    fileprivate func coreTitle(_ title: String) {
        DispatchQueue.main.async { self.title = title }
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
        DispatchQueue.main.async { UIPasteboard.general.string = text }
    }
    func onFocusInput() {
        DispatchQueue.main.async { self.model?.focusKeyboard() }
    }
    func onImeAnchor(left: Double, top: Double, width: Double, height: Double) {
        DispatchQueue.main.async {
            self.model?.cursorRect = CGRect(x: left, y: top, width: width, height: height)
        }
    }
    func onHostKey(fingerprint: String) { model?.coreHostKey(fingerprint) }
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
