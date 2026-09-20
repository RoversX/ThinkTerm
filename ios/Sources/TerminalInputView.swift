import Combine
import SwiftUI
import UIKit

/// The keyboard's target and the terminal's touch surface: an invisible
/// view over the Metal layer that adopts `UITextInput` so the soft
/// keyboard, its IMEs (Pinyin, Kana...) and a hardware keyboard all talk
/// to it, and whose gestures are the terminal's -- a tap focuses the pane
/// under it and raises the keyboard, a one-finger drag scrolls, a
/// two-finger swipe down puts the keyboard away, and a pinch changes the
/// font. Selecting text is the system's own: a UITextInteraction over
/// this view brings the native handles, magnifier and Copy menu, and
/// reads the terminal through UITextInput -- the focused pane's visible
/// rows are the document (`App::screen_text`), with the IME's marked
/// text appended after them. A phone can tell a tap from a drag, so
/// there is no keyboard button.
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
    /// The document the text system reads: the pane's visible rows, as
    /// the core last gave them. Fetched when a selection interaction asks
    /// and dropped when the core reports a change.
    private var screen: ScreenText?
    /// The system's selection, held so the gesture setting can take it
    /// away and put it back while the terminal stays open.
    private let selection = UITextInteraction(for: .editable)
    /// A caret the system placed with a tap: not drawn, but kept, so the
    /// loupe and the handles that follow have a position to start from.
    private var caret: Int?
    private var selectionWatch: AnyCancellable?

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
        let pinch = UIPinchGestureRecognizer(target: self, action: #selector(pinched(_:)))
        let hide = UISwipeGestureRecognizer(target: self, action: #selector(swipedDown(_:)))
        hide.direction = .down
        hide.numberOfTouchesRequired = 2
        // Ours never hold the system's back: the touches go on to the text
        // interaction's recognizers whatever ours decide.
        tap.cancelsTouchesInView = false
        tap.delaysTouchesEnded = false
        // A sideways flick goes to the next or previous tab. The scroll
        // view only moves vertically, so a horizontal pan is nobody's.
        let flick = UIPanGestureRecognizer(target: self, action: #selector(flicked(_:)))
        flick.maximumNumberOfTouches = 1
        flick.cancelsTouchesInView = false
        for g in [tap, pinch, hide, flick] as [UIGestureRecognizer] {
            g.delegate = self
            addGestureRecognizer(g)
        }
        // The system's text editing over the rows, the way every editable
        // text view has it: a tap raises the keyboard, a double tap selects
        // a word and shows the handles, a long press the loupe. The caret
        // it places is kept but not drawn; the terminal's cursor is its own.
        selection.textInput = self
        // The setting decides whether it is attached at all; the publisher
        // hands over the value it holds now, so this also starts it right.
        selectionWatch = AppSettings.shared.$longPressSelects.sink { [weak self] on in
            guard let self else { return }
            let attached = self.interactions.contains { $0 === self.selection }
            if on, !attached {
                self.addInteraction(self.selection)
            } else if !on, attached {
                self.removeInteraction(self.selection)
            }
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

    /// A gesture's point in the view's own box. The scroll view's
    /// coordinates start at the content offset, half-way down the runway.
    private func point(of g: UIGestureRecognizer) -> CGPoint {
        let p = g.location(in: self)
        return CGPoint(x: p.x - contentOffset.x, y: p.y - contentOffset.y)
    }

    @objc private func tapped(_ g: UITapGestureRecognizer) {
        let p = point(of: g)
        // A press and release at the point: the App focuses the pane.
        model?.pointer("down", at: p)
        model?.pointer("up", at: p)
        if !isFirstResponder {
            becomeFirstResponder()
        }
    }

    @objc private func flicked(_ g: UIPanGestureRecognizer) {
        guard g.state == .ended else { return }
        let t = g.translation(in: self)
        let v = g.velocity(in: self)
        guard abs(t.x) > 70, abs(t.y) < 50, abs(v.x) > 400, abs(v.x) > abs(v.y) * 2 else { return }
        model?.switchTab(by: t.x < 0 ? 1 : -1)
    }

    @objc private func swipedDown(_ g: UISwipeGestureRecognizer) {
        // Read the setting as the gesture fires, so turning it off in
        // Settings takes hold without rebuilding the view.
        guard AppSettings.shared.twoFingerHidesKeyboard else { return }
        if isFirstResponder {
            resignFirstResponder()
        }
    }

    @objc private func pinched(_ g: UIPinchGestureRecognizer) {
        // Same as the swipe: the recognizer stays, the setting decides
        // here, so a change applies to the very next pinch.
        guard AppSettings.shared.pinchZoom else { return }
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

    func gestureRecognizer(_ a: UIGestureRecognizer, shouldRequireFailureOf b: UIGestureRecognizer) -> Bool {
        false
    }

    func gestureRecognizer(_ a: UIGestureRecognizer, shouldBeRequiredToFailBy b: UIGestureRecognizer) -> Bool {
        false
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

    // MARK: the document

    /// The core changed something on screen: the rows are read again
    /// when next asked for.
    func screenChanged() {
        inputDelegate?.textWillChange(self)
        screen = nil
        inputDelegate?.textDidChange(self)
    }

    private func currentScreen() -> ScreenText? {
        if let screen { return screen }
        guard let model, let s = ViewJSON.decode(ScreenText.self, model.core.screenText()) else { return nil }
        screen = s
        return s
    }

    /// The rows' length in the document; marked text follows it.
    private var screenLength: Int {
        currentScreen().map { $0.rows * ($0.cols + 1) } ?? 0
    }

    private var markedLength: Int { marked?.utf16.count ?? 0 }
    private var documentLength: Int { screenLength + markedLength }

    private func cell(at index: Int) -> (row: Int, col: Int)? {
        guard let s = currentScreen(), index >= 0, index < screenLength else { return nil }
        let stride = s.cols + 1
        return (index / stride, min(index % stride, s.cols - 1))
    }

    private func index(row: Int, col: Int) -> Int {
        guard let s = currentScreen() else { return 0 }
        return row * (s.cols + 1) + col
    }

    /// The cell under a point UIKit hands the text input, clamped into
    /// the pane. Those points are in the scroll view's coordinates, which
    /// start at the content offset, half-way down the runway.
    private func cell(under point: CGPoint) -> (row: Int, col: Int)? {
        guard let s = currentScreen(), s.cell[0] > 0, s.cell[1] > 0 else { return nil }
        let x = point.x - contentOffset.x
        let y = point.y - contentOffset.y
        let col = Int(((x - s.origin[0]) / s.cell[0]).rounded(.down))
        let row = Int(((y - s.origin[1]) / s.cell[1]).rounded(.down))
        return (min(max(row, 0), s.rows - 1), min(max(col, 0), s.cols - 1))
    }

    /// A cell's rect for UIKit, in the scroll view's coordinates.
    private func rect(row: Int, col: Int, cols: Int = 1) -> CGRect {
        guard let s = currentScreen() else { return .zero }
        return CGRect(
            x: contentOffset.x + s.origin[0] + Double(col) * s.cell[0],
            y: contentOffset.y + s.origin[1] + Double(row) * s.cell[1],
            width: Double(cols) * s.cell[0],
            height: s.cell[1]
        )
    }

    /// The App's cursor rect, in the scroll view's coordinates.
    private var cursorRectForText: CGRect {
        (model?.cursorRect ?? .zero).offsetBy(dx: contentOffset.x, dy: contentOffset.y)
    }

    // MARK: UITextInput -- marked text (after the rows)

    var markedTextRange: UITextRange? {
        guard let marked, !marked.isEmpty else { return nil }
        let n = screenLength
        return Range(n, n + marked.utf16.count)
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

    // MARK: UITextInput -- the selection

    /// The pane's selection while there is one, else the caret at the
    /// end of the marked text (the keyboard's insertion point).
    var selectedTextRange: UITextRange? {
        get {
            if marked == nil, let s = currentScreen(), let sel = s.selection {
                return Range(index(row: sel[0][0], col: sel[0][1]), index(row: sel[1][0], col: sel[1][1]) + 1)
            }
            if marked == nil, let caret, caret <= screenLength {
                return Range(caret, caret)
            }
            let n = documentLength
            return Range(n, n)
        }
        set {
            guard let r = newValue as? Range else {
                caret = nil
                model?.core.clearSelection()
                screen = nil
                return
            }
            // A non-empty range inside the rows is a selection; a caret
            // inside them is remembered and clears it. The end is
            // exclusive here and inclusive in the core.
            if r.b > r.a, let a = cell(at: r.a), let b = cell(at: r.b - 1) {
                caret = nil
                model?.core.setSelection(anchorRow: UInt32(a.row), anchorCol: UInt32(a.col), headRow: UInt32(b.row), headCol: UInt32(b.col))
                if var s = screen {
                    s.selection = [[a.row, a.col], [b.row, b.col]]
                    screen = s
                }
            } else {
                caret = r.a < screenLength ? r.a : nil
                model?.core.clearSelection()
                if var s = screen {
                    s.selection = nil
                    screen = s
                }
            }
        }
    }

    var beginningOfDocument: UITextPosition { Pos(0) }
    var endOfDocument: UITextPosition { Pos(documentLength) }

    func text(in range: UITextRange) -> String? {
        guard let r = range as? Range, r.a <= r.b else { return nil }
        let n = screenLength
        var out = ""
        if r.a < n, let s = currentScreen() {
            let u = s.text.utf16
            let end = min(r.b, n)
            if let a = u.index(u.startIndex, offsetBy: r.a, limitedBy: u.endIndex),
               let b = u.index(u.startIndex, offsetBy: end, limitedBy: u.endIndex) {
                out += String(u[a..<b]) ?? ""
            }
        }
        if r.b > n, let marked {
            let m = marked.utf16
            let a = max(r.a - n, 0), b = min(r.b - n, m.count)
            if a < b {
                out += String(m[m.index(m.startIndex, offsetBy: a)..<m.index(m.startIndex, offsetBy: b)]) ?? ""
            }
        }
        return out
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
        return (0...documentLength).contains(n) ? Pos(n) : nil
    }

    func position(from position: UITextPosition, in direction: UITextLayoutDirection, offset: Int) -> UITextPosition? {
        guard let p = position as? Pos else { return nil }
        switch direction {
        case .right: return self.position(from: p, offset: offset)
        case .left: return self.position(from: p, offset: -offset)
        case .down, .up:
            // A row at a time within the rows.
            guard let s = currentScreen(), let c = cell(at: p.i) else { return nil }
            let row = direction == .down ? c.row + offset : c.row - offset
            guard (0..<s.rows).contains(row) else { return nil }
            return Pos(index(row: row, col: c.col))
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

    /// UIKit's own listener for the text changing under it: the native
    /// selection redraws its handles and highlight when told.
    weak var inputDelegate: UITextInputDelegate?

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
        switch direction {
        case .right, .down: return Range(p.i, min(p.i + 1, documentLength))
        default: return Range(max(p.i - 1, 0), p.i)
        }
    }

    func baseWritingDirection(for position: UITextPosition, in direction: UITextStorageDirection) -> NSWritingDirection {
        .leftToRight
    }

    func setBaseWritingDirection(_ writingDirection: NSWritingDirection, for range: UITextRange) {}

    // MARK: UITextInput -- geometry

    func firstRect(for range: UITextRange) -> CGRect {
        guard let r = range as? Range, let a = cell(at: r.a) else { return cursorRectForText }
        let last = cell(at: max(r.b - 1, r.a)) ?? a
        let cols = last.row == a.row ? last.col - a.col + 1 : (currentScreen()?.cols ?? 1) - a.col
        return rect(row: a.row, col: a.col, cols: max(cols, 1))
    }

    /// Where the caret would be: the cell, but nothing wide enough to
    /// see -- the terminal draws its own cursor, and a second one over a
    /// tapped cell would only mislead.
    func caretRect(for position: UITextPosition) -> CGRect {
        guard let p = position as? Pos, let c = cell(at: p.i) else {
            var r = cursorRectForText
            r.size.width = 0
            return r
        }
        var r = rect(row: c.row, col: c.col)
        r.size.width = 0
        return r
    }

    /// One rect per row the range touches, so the handles sit on the
    /// first and last cells and the highlight follows the rows.
    func selectionRects(for range: UITextRange) -> [UITextSelectionRect] {
        guard let r = range as? Range, r.b > r.a, let s = currentScreen(),
              let a = cell(at: r.a), let b = cell(at: r.b - 1) else { return [] }
        var rects: [UITextSelectionRect] = []
        for row in a.row...b.row {
            let from = row == a.row ? a.col : 0
            let to = row == b.row ? b.col : s.cols - 1
            rects.append(SelectionRect(
                rect: rect(row: row, col: from, cols: to - from + 1),
                start: row == a.row, end: row == b.row
            ))
        }
        return rects
    }

    func closestPosition(to point: CGPoint) -> UITextPosition? {
        guard let c = cell(under: point) else { return endOfDocument }
        return Pos(index(row: c.row, col: c.col))
    }

    func closestPosition(to point: CGPoint, within range: UITextRange) -> UITextPosition? {
        guard let r = range as? Range, let p = closestPosition(to: point) as? Pos else { return nil }
        return Pos(min(max(p.i, r.a), r.b))
    }

    func characterRange(at point: CGPoint) -> UITextRange? {
        guard let p = closestPosition(to: point) as? Pos, p.i < screenLength else { return nil }
        return Range(p.i, p.i + 1)
    }

    // MARK: the edit menu

    override func canPerformAction(_ action: Selector, withSender sender: Any?) -> Bool {
        switch action {
        case #selector(copy(_:)): return currentScreen()?.selection != nil
        case #selector(paste(_:)): return UIPasteboard.general.hasStrings
        case #selector(selectAll(_:)): return currentScreen() != nil
        default: return false
        }
    }

    override func copy(_ sender: Any?) {
        if let text = model?.core.selectedText(), !text.isEmpty {
            UIPasteboard.general.string = text
            model?.showCopied()
        }
        model?.core.clearSelection()
        screen = nil
    }

    override func paste(_ sender: Any?) {
        model?.pasteFromClipboard()
    }

    override func selectAll(_ sender: Any?) {
        guard let s = currentScreen() else { return }
        selectedTextRange = Range(0, index(row: s.rows - 1, col: s.cols - 1) + 1)
    }

    // MARK: traits: a terminal wants raw keys, no autocorrect

    // Stored, not computed: the traits protocol declares them settable,
    // and UIKit reads them through the setter's counterpart. The inline
    // prediction is the one that draws the suggestion bar on iOS 17+.
    var keyboardType: UIKeyboardType = .asciiCapable
    var autocorrectionType: UITextAutocorrectionType = .no
    var autocapitalizationType: UITextAutocapitalizationType = .none
    var spellCheckingType: UITextSpellCheckingType = .no
    var smartQuotesType: UITextSmartQuotesType = .no
    var smartDashesType: UITextSmartDashesType = .no
    var smartInsertDeleteType: UITextSmartInsertDeleteType = .no
    var inlinePredictionType: UITextInlinePredictionType = .no
    var returnKeyType: UIReturnKeyType = .default
    var enablesReturnKeyAutomatically: Bool = false

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

    final class SelectionRect: UITextSelectionRect {
        let r: CGRect
        let first: Bool
        let last: Bool
        init(rect: CGRect, start: Bool, end: Bool) { r = rect; first = start; last = end }
        override var rect: CGRect { r }
        override var writingDirection: NSWritingDirection { .leftToRight }
        override var containsStart: Bool { first }
        override var containsEnd: Bool { last }
        override var isVertical: Bool { false }
    }
}

/// `App::screen_text`, as the core hands it over.
struct ScreenText: Decodable {
    var cols: Int
    var rows: Int
    var text: String
    var origin: [Double]
    var cell: [Double]
    var selection: [[Int]]?
}

struct TerminalInput: UIViewRepresentable {
    @EnvironmentObject var model: TerminalModel

    func makeUIView(context: Context) -> TerminalInputView {
        let view = TerminalInputView()
        view.model = model
        model.inputView = view
        return view
    }

    func updateUIView(_ uiView: TerminalInputView, context: Context) {}
}
