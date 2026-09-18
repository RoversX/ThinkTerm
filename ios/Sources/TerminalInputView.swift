import SwiftUI
import UIKit

/// The keyboard's target: an invisible view over the terminal that adopts
/// `UITextInput` so the soft keyboard, its IMEs (Pinyin, Kana...) and a
/// hardware keyboard all talk to it. It holds no text of its own beyond
/// the IME's marked (composing) string: committed text goes straight to
/// the core, keys go as key events, and while a composition is open the
/// core is told to leave the keyboard to the IME.
///
/// The rules this enforces (plan §2.6):
/// - nothing is sent while composing; the marked text is the IME's;
/// - cancelling a composition sends nothing;
/// - picking a candidate commits once (insertText clears the marked text
///   before unmarkText can commit it again);
/// - backspace inside a composition edits the marked text only.
final class TerminalInputView: UIView, UITextInput {
    weak var model: ProbeModel?

    /// The IME's composing text, `nil` when none is open.
    private(set) var marked: String?
    /// Selection inside the marked text, for the IME's own bookkeeping.
    private var markedSelection = NSRange(location: 0, length: 0)

    override init(frame: CGRect) {
        super.init(frame: frame)
        isUserInteractionEnabled = true
        backgroundColor = .clear
        addGestureRecognizer(UITapGestureRecognizer(target: self, action: #selector(tapped)))
    }

    required init?(coder: NSCoder) { fatalError() }

    @objc private func tapped() {
        if !isFirstResponder {
            becomeFirstResponder()
        }
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

    /// Called by the shell when the pane goes away or the connection
    /// changes: an open composition must not land in whatever comes next.
    func dropComposition() {
        endComposing()
    }

    // MARK: UIKeyInput

    var hasText: Bool { marked?.isEmpty == false }

    func insertText(_ text: String) {
        // A candidate was chosen (or plain text typed with no IME): the
        // marked text is replaced by this, and this alone is sent.
        endComposing()
        if text == "\n" {
            model?.key("Enter")
        } else {
            model?.core.text(text: text)
        }
    }

    func deleteBackward() {
        // Inside a composition the IME edits its own buffer through
        // setMarkedText; this only fires for a real backspace.
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
            // The IME cleared its buffer without committing: a cancel.
            endComposing()
            return
        }
        beginComposing()
        marked = text
        markedSelection = selectedRange
        model?.compositionChanged(text)
    }

    func unmarkText() {
        // The composition ended with text still marked (the IME committed
        // the raw buffer, or the keyboard went away): commit it once.
        if let marked, !marked.isEmpty {
            self.marked = nil
            model?.core.setComposing(on: false)
            model?.compositionChanged(nil)
            model?.core.text(text: marked)
            return
        }
        endComposing()
    }

    // MARK: UITextInput -- the (empty) document

    // The view's "text" is only ever the marked string; everything else
    // has been handed to the terminal. Positions are UTF-16 offsets into
    // the marked text, and the document is otherwise empty.

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
        // Replacing inside the marked text is the IME's business and comes
        // through setMarkedText; anything else is a commit.
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

    // The candidate bar and the magnifier ask where the text is; the
    // terminal's cursor rectangle is what they should get. The model keeps
    // it from the last frame (in points, in this view's coordinates).
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
        let arrows: [(String, String)] = [
            (UIKeyCommand.inputUpArrow, "ArrowUp"), (UIKeyCommand.inputDownArrow, "ArrowDown"),
            (UIKeyCommand.inputLeftArrow, "ArrowLeft"), (UIKeyCommand.inputRightArrow, "ArrowRight"),
            (UIKeyCommand.inputEscape, "Escape"), ("\t", "Tab"),
        ]
        for (input, _) in arrows {
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
    @EnvironmentObject var model: ProbeModel

    func makeUIView(context: Context) -> TerminalInputView {
        let view = TerminalInputView()
        view.model = model
        model.inputView = view
        return view
    }

    func updateUIView(_ uiView: TerminalInputView, context: Context) {}
}
