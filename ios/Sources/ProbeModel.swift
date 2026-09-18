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
final class ProbeModel: ObservableObject, @unchecked Sendable {
    @Published var attached = false
    @Published var animating = false
    @Published var inset: CGFloat = 0
    @Published var stats = ""
    @Published var status = ""
    @Published var logText = ""
    @Published var host = "127.0.0.1"
    @Published var port = "2299"
    @Published var user = NSUserName()
    @Published var input = ""
    /// The IME's composing text, shown above the terminal while open.
    @Published var composing: String?
    weak var inputView: TerminalInputView?
    /// Where the candidate bar should hang, in the input view's points.
    /// Fixed until the core reports the cursor's rectangle (S1).
    var cursorRect = CGRect(x: 8, y: 8, width: 2, height: 20)

    let core: Core
    private let frameNeeded = AtomicFlag()
    private var displayLink: CADisplayLink?
    private var statsTimer: Timer?
    private var logLines: [String] = []

    /// The layer currently attached, and the generation the core gave it.
    weak var layer: CAMetalLayer?
    private(set) var generation: UInt64 = 0
    private var wantsSurface = false
    private var lastSize: (Int, Int, Double) = (0, 0, 0)

    // The probe's ssh setup: a throwaway key and a wrapper that points the
    // remote proxy at an isolated HOME. Overridable from the launch
    // arguments so a real host can be tried without a rebuild.
    var keyPath = "/tmp/ttp-ssh/userkey"
    var remoteCommand = "/tmp/ttp-ssh/thinkterm-remote cli --prefer-mux proxy"

    init() {
        let sink = NotifySink()
        core = Core(notify: sink)
        sink.model = self

        let args = ProcessInfo.processInfo.arguments
        func arg(_ name: String) -> String? {
            guard let i = args.firstIndex(of: name), i + 1 < args.count else { return nil }
            return args[i + 1]
        }
        if let h = arg("--host") { host = h }
        if let p = arg("--port") { port = p }
        if let u = arg("--user") { user = u }
        if let k = arg("--key") { keyPath = k }
        if let c = arg("--remote-command") { remoteCommand = c }

        let link = CADisplayLink(target: self, selector: #selector(tick))
        link.add(to: .main, forMode: .common)
        displayLink = link
        statsTimer = Timer.scheduledTimer(withTimeInterval: 0.5, repeats: true) { [weak self] _ in
            guard let self else { return }
            self.stats = self.core.stats()
        }
        if args.contains("--autotest") {
            scheduleAutotest()
        }
        if args.contains("--autoconnect") {
            scheduleAutoconnect()
        }
        if args.contains("--imetest") {
            scheduleImeTest()
        }
    }

    func compositionChanged(_ text: String?) {
        DispatchQueue.main.async { self.composing = text }
    }

    func focusKeyboard() {
        inputView?.becomeFirstResponder()
    }

    func dismissKeyboard() {
        inputView?.resignFirstResponder()
    }

    // MARK: connection

    var fontPaths: [String] {
        ["JetBrainsMono-Regular", "SymbolsNerdFontMono-Regular"].compactMap {
            Bundle.main.path(forResource: $0, ofType: "ttf")
        }
    }

    func connect() {
        let paths = fontPaths
        guard paths.count == 2 else {
            log("shell: fonts missing from the bundle (\(paths))")
            return
        }
        core.connect(
            host: host,
            port: UInt16(port) ?? 22,
            user: user,
            keyPath: keyPath,
            remoteCommand: remoteCommand,
            fontPaths: paths,
            sizePt: 11.0,
            painter: CoreTextPainter()
        )
    }

    func disconnect() { core.disconnect() }

    func send() {
        guard !input.isEmpty else { return }
        core.text(text: input)
        input = ""
    }

    func sendLine() {
        core.text(text: input)
        core.key(name: "Enter", ctrl: false, alt: false, shift: false)
        input = ""
    }

    func key(_ name: String, ctrl: Bool = false, alt: Bool = false, shift: Bool = false) {
        core.key(name: name, ctrl: ctrl, alt: alt, shift: shift)
    }

    func scroll(_ lines: Int32) { core.scroll(lines: lines) }

    // MARK: autotest (driven by launch arguments; results go to the console)

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

    /// Connect, wait for the pane, type a line with CJK and an emoji so the
    /// painter path is exercised, then report. The screenshot is taken from
    /// outside.
    private func scheduleAutoconnect() {
        let q = DispatchQueue.main
        q.asyncAfter(deadline: .now() + 1.5) { self.connect(); self.report("connect sent") }
        q.asyncAfter(deadline: .now() + 5.0) { self.report("after connect") }
        q.asyncAfter(deadline: .now() + 5.2) {
            self.input = "echo '你好, 世界 🙂 hello from iOS'"
            self.sendLine()
        }
        q.asyncAfter(deadline: .now() + 7.5) { self.report("after typing") }
        // Output that arrives while nothing is being drawn must wake a frame.
        q.asyncAfter(deadline: .now() + 7.6) {
            self.input = "sleep 2; echo WOKE-BY-OUTPUT"
            self.sendLine()
        }
        q.asyncAfter(deadline: .now() + 8.6) { self.report("before wake") }
        q.asyncAfter(deadline: .now() + 11.0) { self.report("after wake") }
        // The surface goes and comes back while the session stays.
        q.asyncAfter(deadline: .now() + 11.2) { self.cycleAttachDetach(times: 5); self.report("after 5 cycles connected") }
        q.asyncAfter(deadline: .now() + 12.5) { self.report("final") }
        // The harness kills the remote proxy at about 13 s; the core must
        // dial again and come back with the same pane.
        q.asyncAfter(deadline: .now() + 15.0) { self.report("during reconnect") }
        q.asyncAfter(deadline: .now() + 19.0) {
            self.input = "echo AFTER-RECONNECT"
            self.sendLine()
        }
        q.asyncAfter(deadline: .now() + 21.0) { self.report("after reconnect"); print("AUTOCONNECT DONE") }
    }

    /// Drives the UITextInput view the way an IME does, without one: the
    /// simulator cannot be typed into from outside, so the view's contract
    /// is exercised directly and the core's input counter is checked at
    /// each step. A real Pinyin keyboard is the user's to try.
    private func scheduleImeTest() {
        let q = DispatchQueue.main
        var sent: Int { Int(inputsSent()) }
        var sizeBefore = ""
        q.asyncAfter(deadline: .now() + 1.5) { self.connect() }
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
                // Candidate chosen: iOS calls insertText then unmarkText.
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
        let line = "IMECHECK \(ok ? "PASS" : "FAIL") \(name): \(detail)"
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

    // MARK: surface lifecycle, all on the main thread

    /// Called by the view whenever its layer exists and has a size.
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

    /// Called by the view before its layer goes away.
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
        // The one synchronous call: returns once the core has let go.
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
    }

    func requestOneFrame() { frameNeeded.set() }

    func toggleAnimating() {
        animating.toggle()
        core.setAnimating(on: animating)
    }

    func toggleInset() {
        inset = inset > 0 ? 0 : 60
    }

    // MARK: notify (core thread)

    fileprivate func coreWantsFrame() {
        frameNeeded.set()
    }

    fileprivate func coreStatus(_ status: String) {
        DispatchQueue.main.async {
            self.status = status
            self.log("status: " + status)
            // Whatever the IME was composing belongs to the pane that was
            // there; a new attachment must not receive it.
            if status.hasPrefix("connecting") || status.contains("reconnect") || status.hasPrefix("disconnected") {
                self.inputView?.dropComposition()
            }
        }
    }

    fileprivate func coreLogged(_ line: String) {
        print("core: " + line)
        DispatchQueue.main.async { self.log("core: " + line) }
    }

    private func log(_ line: String) {
        logLines.append(line)
        if logLines.count > 40 { logLines.removeFirst(logLines.count - 40) }
        logText = logLines.joined(separator: "\n")
    }
}

/// UniFFI hands the callback object to the core thread; it forwards to the
/// model without touching anything that needs the main thread.
private final class NotifySink: Notify, @unchecked Sendable {
    weak var model: ProbeModel?
    func onFrameNeeded() { model?.coreWantsFrame() }
    func onStatus(status: String) { model?.coreStatus(status) }
    func onLog(line: String) { model?.coreLogged(line) }
}

/// Paints the graphemes the bundled faces lack with CoreText, which falls
/// through the system's cascade list (PingFang for Han, Apple Color Emoji
/// for emoji) on its own. Called on the core thread.
final class CoreTextPainter: GlyphPainter, @unchecked Sendable {
    func paint(text: String, px: Double, width: UInt32, height: UInt32, penX: Double, baseline: Double, red: UInt8, green: UInt8, blue: UInt8) -> Data {
        let w = Int(width), h = Int(height)
        guard w > 0, h > 0 else { return Data() }
        let colorSpace = CGColorSpaceCreateDeviceRGB()
        // CoreGraphics only draws into premultiplied contexts; the bytes are
        // un-premultiplied below, since the atlas expects straight alpha.
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
