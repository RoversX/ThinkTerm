import SwiftUI
import UIKit

/// The keyboard's target and the terminal's touch surface: an invisible
/// view over the Metal layer that adopts `UITextInput` so the soft
/// keyboard, its IMEs (Pinyin, Kana...) and a hardware keyboard all talk
/// to it, and whose gestures are the terminal's -- a tap focuses the pane
/// under it and raises the keyboard, a one-finger drag scrolls, a
/// two-finger swipe down puts the keyboard away, a long press opens the
/// pane's menu, a pinch changes the font. A phone can tell a tap from a
/// drag, so there is no keyboard button.
///
/// It holds no text of its own beyond the IME's marked (composing)
/// string: committed text goes straight to the core, keys go as key
/// events, and while a composition is open the core is told to leave the
/// keyboard to the IME. The rules (plan §2.6): nothing is sent while
/// composing; cancelling sends nothing; a candidate commits once
/// (insertText clears the marked text before unmarkText can commit it
/// again); backspace inside a composition edits the marked text only.
final class TerminalInputView: UIScrollView, UITextInput, UIGestureRecognizerDelegate, UIScrollViewDelegate {
    weak var model: TerminalModel?
    var onMenu: ((CGPoint) -> Void)?

    private(set) var marked: String?
    private var markedSelection = NSRange(location: 0, length: 0)
    private var pinchStart: CGFloat = 1
    /// The scroll view is the drag surface: its content is a tall nothing,
    /// the offset's travel is what the terminal scrolls by, in points, and
    /// the view brings iOS's own inertia and deceleration with it. The
    /// offset is parked mid-way and put back there whenever a scroll ends.
    private let runway: CGFloat = 200_000
    private var lastOffset: CGFloat = 0
    private var recentring = false
    /// Stepped mode: finger travel not yet a whole row.
    private var rowRemainder: CGFloat = 0

    override init(frame: CGRect) {
        super.init(frame: frame)
        isUserInteractionEnabled = true
        backgroundColor = .clear
        showsVerticalScrollIndicator = false
        showsHorizontalScrollIndicator = false
        bounces = false
        alwaysBounceVertical = false
        isDirectionalLockEnabled = true
        delaysContentTouches = false
        panGestureRecognizer.maximumNumberOfTouches = 1
        delegate = self
        let tap = UITapGestureRecognizer(target: self, action: #selector(tapped(_:)))
        let press = UILongPressGestureRecognizer(target: self, action: #selector(pressed(_:)))
        let pinch = UIPinchGestureRecognizer(target: self, action: #selector(pinched(_:)))
        let hide = UISwipeGestureRecognizer(target: self, action: #selector(swipedDown(_:)))
        press.minimumPressDuration = 0.45
        hide.direction = .down
        hide.numberOfTouchesRequired = 2
        for g in [tap, press, pinch, hide] as [UIGestureRecognizer] {
            g.delegate = self
            addGestureRecognizer(g)
        }
    }

    override func layoutSubviews() {
        super.layoutSubviews()
        let size = CGSize(width: bounds.width, height: runway)
        if contentSize != size {
            contentSize = size
            recentre()
        }
    }

    private func recentre() {
        recentring = true
        contentOffset = CGPoint(x: 0, y: runway / 2)
        lastOffset = contentOffset.y
        recentring = false
    }

    // MARK: UIScrollViewDelegate: the drag, with inertia

    func scrollViewDidScroll(_ scrollView: UIScrollView) {
        if recentring { return }
        let y = contentOffset.y
        let delta = y - lastOffset
        lastOffset = y
        guard delta != 0, let model else { return }
        // The bounds' origin is the content offset; the App wants a point
        // in the view, so the centre is measured from the top-left corner.
        let centre = CGPoint(x: bounds.width / 2, y: bounds.height / 2)
        if model.smoothScroll {
            model.wheelPx(at: centre, px: Double(delta))
        } else {
            // Whole rows, at the App's own row height.
            let row = CGFloat(max(model.cellHeight, 1))
            rowRemainder += delta
            let rows = (rowRemainder / row).rounded(.towardZero)
            if rows != 0 {
                rowRemainder -= rows * row
                // The App's stepped mode counts a wheel event as one notch
                // per line asked; the shell scrolls history up by asking
                // for negative lines.
                model.wheel(at: centre, lines: Double(-rows))
            }
        }
    }

    func scrollViewDidEndDragging(_ scrollView: UIScrollView, willDecelerate decelerate: Bool) {
        if !decelerate { recentre() }
        if !(model?.smoothScroll ?? true) { rowRemainder = 0 }
    }

    func scrollViewDidEndDecelerating(_ scrollView: UIScrollView) {
        recentre()
    }

    required init?(coder: NSCoder) { fatalError() }

    // MARK: gestures

    @objc private func tapped(_ g: UITapGestureRecognizer) {
        let p = g.location(in: self)
        // A press and release at the point: the App focuses the pane.
        model?.pointer("down", at: p)
        model?.pointer("up", at: p)
        if !isFirstResponder {
            becomeFirstResponder()
        }
    }

    @objc private func swipedDown(_ g: UISwipeGestureRecognizer) {
        if isFirstResponder {
            resignFirstResponder()
        }
    }

    @objc private func pressed(_ g: UILongPressGestureRecognizer) {
        guard g.state == .began else { return }
        let p = g.location(in: self)
        model?.pointer("down", at: p)
        model?.pointer("up", at: p)
        onMenu?(p)
    }

    @objc private func pinched(_ g: UIPinchGestureRecognizer) {
        switch g.state {
        case .began: pinchStart = 1
        case .changed:
            // One font step per 15% of scale, either way.
            while g.scale > pinchStart * 1.15 {
                pinchStart *= 1.15
                model?.stepFont(1)
            }
            while g.scale < pinchStart / 1.15 {
                pinchStart /= 1.15
                model?.stepFont(-1)
            }
        default: break
        }
    }

    func gestureRecognizer(_ a: UIGestureRecognizer, shouldRecognizeSimultaneouslyWith b: UIGestureRecognizer) -> Bool {
        true
    }

    override var canBecomeFirstResponder: Bool { true }

    // MARK: composition bookkeeping

    private func beginComposing() {
        if marked == nil {
            model?.core.setComposing(on: true)
        }
    }

    private func endComposing() {
        if marked != nil {
            marked = nil
            markedSelection = NSRange(location: 0, length: 0)
            model?.core.setComposing(on: false)
        }
        model?.compositionChanged(nil)
    }

    /// The pane went away or the connection changed: an open composition
    /// must not land in whatever comes next.
    func dropComposition() {
        endComposing()
    }

    // MARK: UIKeyInput

    var hasText: Bool { marked?.isEmpty == false }

    func insertText(_ text: String) {
        endComposing()
        if text == "\n" {
            model?.key("Enter")
        } else {
            model?.text(text)
        }
    }

    func deleteBackward() {
        if marked != nil { return }
        model?.key("Backspace")
    }

    // MARK: UITextInput -- marked text

    var markedTextRange: UITextRange? {
        guard let marked, !marked.isEmpty else { return nil }
        return Range(0, marked.utf16.count)
    }

    var markedTextStyle: [NSAttributedString.Key: Any]? {
        get { nil }
        set {}
    }

    func setMarkedText(_ markedText: String?, selectedRange: NSRange) {
        let text = markedText ?? ""
        if text.isEmpty {
            endComposing()
            return
        }
        beginComposing()
        marked = text
        markedSelection = selectedRange
        model?.compositionChanged(text)
    }

    func unmarkText() {
        if let marked, !marked.isEmpty {
            self.marked = nil
            model?.core.setComposing(on: false)
            model?.compositionChanged(nil)
            model?.text(marked)
            return
        }
        endComposing()
    }

    // MARK: UITextInput -- the (empty) document

    var selectedTextRange: UITextRange? {
        get {
            let n = marked?.utf16.count ?? 0
            return Range(n, n)
        }
        set {}
    }

    var beginningOfDocument: UITextPosition { Pos(0) }
    var endOfDocument: UITextPosition { Pos(marked?.utf16.count ?? 0) }

    func text(in range: UITextRange) -> String? {
        guard let r = range as? Range, let marked else { return nil }
        let s = marked.utf16
        guard r.a <= s.count, r.b <= s.count, r.a <= r.b else { return nil }
        let start = s.index(s.startIndex, offsetBy: r.a)
        let end = s.index(s.startIndex, offsetBy: r.b)
        return String(s[start..<end])
    }

    func replace(_ range: UITextRange, withText text: String) {
        insertText(text)
    }

    func textRange(from fromPosition: UITextPosition, to toPosition: UITextPosition) -> UITextRange? {
        guard let a = fromPosition as? Pos, let b = toPosition as? Pos else { return nil }
        return Range(min(a.i, b.i), max(a.i, b.i))
    }

    func position(from position: UITextPosition, offset: Int) -> UITextPosition? {
        guard let p = position as? Pos else { return nil }
        let n = p.i + offset
        let max = marked?.utf16.count ?? 0
        return (0...max).contains(n) ? Pos(n) : nil
    }

    func position(from position: UITextPosition, in direction: UITextLayoutDirection, offset: Int) -> UITextPosition? {
        switch direction {
        case .right, .down: return self.position(from: position, offset: offset)
        case .left, .up: return self.position(from: position, offset: -offset)
        @unknown default: return nil
        }
    }

    func compare(_ position: UITextPosition, to other: UITextPosition) -> ComparisonResult {
        guard let a = position as? Pos, let b = other as? Pos else { return .orderedSame }
        return a.i < b.i ? .orderedAscending : a.i > b.i ? .orderedDescending : .orderedSame
    }

    func offset(from: UITextPosition, to toPosition: UITextPosition) -> Int {
        guard let a = from as? Pos, let b = toPosition as? Pos else { return 0 }
        return b.i - a.i
    }

    var inputDelegate: UITextInputDelegate? {
        get { nil }
        set {}
    }

    var tokenizer: UITextInputTokenizer { UITextInputStringTokenizer(textInput: self) }

    func position(within range: UITextRange, farthestIn direction: UITextLayoutDirection) -> UITextPosition? {
        guard let r = range as? Range else { return nil }
        switch direction {
        case .left, .up: return Pos(r.a)
        default: return Pos(r.b)
        }
    }

    func characterRange(byExtending position: UITextPosition, in direction: UITextLayoutDirection) -> UITextRange? {
        guard let p = position as? Pos else { return nil }
        return Range(p.i, p.i)
    }

    func baseWritingDirection(for position: UITextPosition, in direction: UITextStorageDirection) -> NSWritingDirection {
        .leftToRight
    }

    func setBaseWritingDirection(_ writingDirection: NSWritingDirection, for range: UITextRange) {}

    func firstRect(for range: UITextRange) -> CGRect { model?.cursorRect ?? CGRect(x: 0, y: 0, width: 1, height: 20) }
    func caretRect(for position: UITextPosition) -> CGRect { model?.cursorRect ?? CGRect(x: 0, y: 0, width: 1, height: 20) }
    func selectionRects(for range: UITextRange) -> [UITextSelectionRect] { [] }
    func closestPosition(to point: CGPoint) -> UITextPosition? { endOfDocument }
    func closestPosition(to point: CGPoint, within range: UITextRange) -> UITextPosition? { endOfDocument }
    func characterRange(at point: CGPoint) -> UITextRange? { nil }

    // MARK: traits: a terminal wants raw keys, no autocorrect

    var keyboardType: UIKeyboardType { .asciiCapable }
    var autocorrectionType: UITextAutocorrectionType { .no }
    var autocapitalizationType: UITextAutocapitalizationType { .none }
    var spellCheckingType: UITextSpellCheckingType { .no }
    var smartQuotesType: UITextSmartQuotesType { .no }
    var smartDashesType: UITextSmartDashesType { .no }
    var smartInsertDeleteType: UITextSmartInsertDeleteType { .no }
    var returnKeyType: UIReturnKeyType { .default }
    var enablesReturnKeyAutomatically: Bool { false }

    // MARK: hardware keyboard

    override var keyCommands: [UIKeyCommand]? {
        var cmds: [UIKeyCommand] = []
        let specials: [String] = [
            UIKeyCommand.inputUpArrow, UIKeyCommand.inputDownArrow,
            UIKeyCommand.inputLeftArrow, UIKeyCommand.inputRightArrow,
            UIKeyCommand.inputEscape, "\t",
        ]
        for input in specials {
            let c = UIKeyCommand(input: input, modifierFlags: [], action: #selector(hardwareKey(_:)))
            c.wantsPriorityOverSystemBehavior = true
            cmds.append(c)
        }
        for ch in "abcdefghijklmnopqrstuvwxyz[]\\" {
            let c = UIKeyCommand(input: String(ch), modifierFlags: .control, action: #selector(hardwareKey(_:)))
            c.wantsPriorityOverSystemBehavior = true
            cmds.append(c)
        }
        return cmds
    }

    @objc private func hardwareKey(_ cmd: UIKeyCommand) {
        guard let input = cmd.input else { return }
        let name: String
        switch input {
        case UIKeyCommand.inputUpArrow: name = "ArrowUp"
        case UIKeyCommand.inputDownArrow: name = "ArrowDown"
        case UIKeyCommand.inputLeftArrow: name = "ArrowLeft"
        case UIKeyCommand.inputRightArrow: name = "ArrowRight"
        case UIKeyCommand.inputEscape: name = "Escape"
        case "\t": name = "Tab"
        default: name = input
        }
        model?.key(name, ctrl: cmd.modifierFlags.contains(.control), alt: cmd.modifierFlags.contains(.alternate), shift: cmd.modifierFlags.contains(.shift))
    }

    // MARK: positions

    final class Pos: UITextPosition {
        let i: Int
        init(_ i: Int) { self.i = i }
    }

    final class Range: UITextRange {
        let a: Int, b: Int
        init(_ a: Int, _ b: Int) { self.a = a; self.b = b }
        override var isEmpty: Bool { a == b }
        override var start: UITextPosition { Pos(a) }
        override var end: UITextPosition { Pos(b) }
    }
}

struct TerminalInput: UIViewRepresentable {
    @EnvironmentObject var model: TerminalModel
    var onMenu: (CGPoint) -> Void

    func makeUIView(context: Context) -> TerminalInputView {
        let view = TerminalInputView()
        view.model = model
        view.onMenu = onMenu
        model.inputView = view
        return view
    }

    func updateUIView(_ uiView: TerminalInputView, context: Context) {
        uiView.onMenu = onMenu
    }
}
