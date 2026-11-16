import Combine
import SwiftUI
import UIKit
import UIKit.UIGestureRecognizerSubclass

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
/// text appended after them. A long press is the view's own: it selects
/// the word under the finger, which the system's does not do over
/// editable text. A phone can tell a tap from a drag, so there is no
/// keyboard button.
///
/// Committed text goes straight to the core, keys go as key events, and
/// while a composition is open the core is told to leave the keyboard to
/// the IME. The rules (plan §2.6): nothing is sent while composing;
/// cancelling sends nothing; a candidate commits once (insertText clears
/// the marked text before unmarkText can commit it again); backspace
/// inside a composition edits the marked text only.
///
/// What the keyboard typed is also kept, after the rows (`typed`): the
/// keyboard reads its own words back, and dictation goes on only while
/// it finds them there and can put a better hearing in their place.
final class TerminalInputView: UIScrollView, UITextInput, UIGestureRecognizerDelegate, UIScrollViewDelegate, UIEditMenuInteractionDelegate {
    weak var model: TerminalModel?

    private(set) var marked: String?
    private var markedSelection = NSRange(location: 0, length: 0)
    /// What the keyboard typed, as the keyboard knows it: the document's
    /// tail between the rows and the marked text. The part from `settled`
    /// on is still the end of the line being typed, so the keyboard may
    /// change it (`replace`); the part before was entered, or something
    /// else has typed since.
    private(set) var typed = ""
    /// UTF-16 units of `typed` the keyboard can no longer change.
    private var settled = 0
    /// A stretch of `typed` the keyboard selected, in its UTF-16 units:
    /// what it types next takes that stretch's place.
    private var typedSelection: (a: Int, b: Int)?
    /// The view itself is sending: not somebody else's typing.
    private var sending = false
    /// `typed` is cut back to this once the line is entered, and at any
    /// time past four times it.
    private static let typedKept = 2048
    /// A space the keyboard took off the line in this turn of the run
    /// loop (`deleteBackward`): see `periodShortcut`.
    private var spaceTaken = false
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
    private let tap = UITapGestureRecognizer()
    /// When the terminal last moved under a finger or coasted on.
    private var lastScrollAt: CFTimeInterval = 0
    /// How far the finger of the touch at hand has gone.
    private let travel = TravelWatch()
    /// The touch stopped the terminal as it coasted, or began to scroll it.
    private var scrolled = false
    /// A finger that went this far was scrolling, as the scroll view
    /// itself counts it.
    private static let tapSlop: CGFloat = 10
    /// The touch at hand is not a tap, though it ends as one for the tap
    /// recognizers -- the view's and the system's both, which allow a
    /// finger more room than a scroll needs -- and each would raise the
    /// keyboard.
    private var notATap: Bool { scrolled || travel.farthest >= Self.tapSlop }
    /// The document the text system reads: the pane's visible rows,
    /// decoded once and dropped when they change (`currentScreen`).
    private var screen: ScreenText?
    /// The system's selection, held so the gesture setting can take it
    /// away and put it back while the terminal stays open.
    /// Editable, or the keyboard stays down (and UIKit logs "cannot add
    /// handler" for every gesture). The caret it draws takes the view's
    /// tint, which is clear until a selection needs its handles seen.
    private let selection = UITextInteraction(for: .editable)
    /// A caret the system placed with a tap: not drawn, but kept, so the
    /// loupe and the handles that follow have a position to start from.
    private var caret: Int?
    private var selectionWatch: AnyCancellable?
    /// The terminal's own long press. Over editable text the system's only
    /// moves a caret, and this view draws none: here the word under the
    /// finger is selected, a drag on from there takes the selection along,
    /// and lifting brings the menu.
    private let press = UILongPressGestureRecognizer()
    /// What the press first selected, while the finger is still down.
    private var pressAnchor: (a: Int, b: Int)?
    /// The finger is down in a press: the selection is the press's, and
    /// the caret the system's own long press goes on placing is not taken.
    private var pressing = false
    /// First responder for a selection's sake only -- the system draws
    /// one nowhere else -- so no keyboard comes with it, and the rows do
    /// not move from under what was just selected.
    private var quiet = false
    private let noKeyboard = UIView()
    /// The menu for the press: the system shows its own only for what its
    /// own gestures selected.
    private lazy var pressMenu = UIEditMenuInteraction(delegate: self)
    /// The press's menu is up, or on its way out.
    private var pressMenuUp = false
    /// Select All was chosen from it: it comes back for the Copy.
    private var menuAgain = false
    /// A word ends at a blank and at these, as it does on the desktop.
    private static let wordBreaks = Set(" \t\n{[}]()\"'`".utf16)

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
        tap.addTarget(self, action: #selector(tapped(_:)))
        travel.began = { [weak self] in
            guard let self else { return }
            // Landing on the terminal as it coasts stops it.
            self.scrolled = self.isDecelerating || CACurrentMediaTime() - self.lastScrollAt < 0.1
        }
        travel.over = { [weak self] in
            // The taps this touch ends in are told in this same turn:
            // what it was is kept until they have been.
            DispatchQueue.main.async {
                guard let self, !self.travel.tracking else { return }
                self.scrolled = false
                self.travel.forget()
            }
        }
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
        press.addTarget(self, action: #selector(pressed(_:)))
        press.cancelsTouchesInView = false
        for g in [tap, pinch, hide, flick, press, travel] as [UIGestureRecognizer] {
            g.delegate = self
            addGestureRecognizer(g)
        }
        addInteraction(pressMenu)
        // The system's text editing over the rows, the way every editable
        // text view has it: a tap raises the keyboard, a double tap selects
        // a word and shows the handles. Its long press, which would only
        // place a caret under a loupe, waits on the view's own (`pressed`).
        // The caret it places is kept but not drawn; the terminal's cursor
        // is its own.
        selection.textInput = self
        // No caret over the terminal's own cursor: the tint that draws it
        // is clear until a selection puts handles up.
        tintColor = .clear
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
        if delta != 0 { lastScrollAt = CACurrentMediaTime() }
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

    func scrollViewWillBeginDragging(_ scrollView: UIScrollView) {
        scrolled = true
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
        // A tap has no time limit of its own: a press that selected ends
        // in one too, and is not a click.
        if pressing { return }
        // Nor is the lift of a finger that scrolled, or stopped a scroll.
        if notATap { return }
        // Held for a selection alone: a tap elsewhere lets it go, and is
        // neither a click nor a call for the keyboard.
        if quiet {
            resignFirstResponder()
            return
        }
        let p = point(of: g)
        lineDisturbed()
        // A press and release at the point: the App focuses the pane.
        model?.pointer("down", at: p)
        model?.pointer("up", at: p)
        if !isFirstResponder {
            becomeFirstResponder()
        }
    }

    @objc private func pressed(_ g: UILongPressGestureRecognizer) {
        switch g.state {
        case .began:
            // The setting takes the system's selection away altogether;
            // a composition has the document to itself.
            guard AppSettings.shared.longPressSelects, marked == nil else { return }
            pressing = true
            // The system draws a selection, and fills a menu, for the
            // first responder only.
            holdQuietly()
            pressAnchor = word(under: g.location(in: self))
            guard let word = pressAnchor else { return }
            select(word)
            UISelectionFeedbackGenerator().selectionChanged()
        case .changed:
            guard pressing, let anchor = pressAnchor, let c = cell(under: g.location(in: self)) else { return }
            // From the word to the finger, whichever side of it that is.
            let i = index(row: c.row, col: c.col)
            select((min(anchor.a, i), max(anchor.b, i + 1)))
        case .ended, .cancelled, .failed:
            guard pressing else { return }
            let lifted = g.state == .ended
            let at = g.location(in: self)
            pressAnchor = nil
            // The tap every lift ends in is told in this same turn, and
            // so is the system's long press where it ran all the same and
            // places its caret once more: the press is over after them.
            DispatchQueue.main.async { [weak self] in
                guard let self else { return }
                self.pressing = false
                if lifted { self.showPressMenu(at: at) }
            }
        default: break
        }
    }

    /// The word around a point in the focused pane: the cells either
    /// side of it up to a blank or a word boundary. Nil on a blank, on a
    /// boundary, and outside the pane.
    private func word(under point: CGPoint) -> (a: Int, b: Int)? {
        guard let s = currentScreen(), s.cell[0] > 0, s.cell[1] > 0 else { return nil }
        let col = Int(((point.x - contentOffset.x - s.origin[0]) / s.cell[0]).rounded(.down))
        let row = Int(((point.y - contentOffset.y - s.origin[1]) / s.cell[1]).rounded(.down))
        guard (0..<s.cols).contains(col), (0..<s.rows).contains(row) else { return nil }
        let start = row * (s.cols + 1)
        let units = Array(s.text.utf16.dropFirst(start).prefix(s.cols))
        guard units.count == s.cols else { return nil }
        let inWord = { (c: Int) in !Self.wordBreaks.contains(units[c]) }
        guard inWord(col) else { return nil }
        var a = col, b = col + 1
        while a > 0, inWord(a - 1) { a -= 1 }
        while b < s.cols, inWord(b) { b += 1 }
        return (start + a, start + b)
    }

    /// Puts the selection where the view itself has it, and has the
    /// system draw it there.
    private func select(_ range: (a: Int, b: Int)) {
        if let selected, selected.a == range.a, selected.b == range.b { return }
        inputDelegate?.selectionWillChange(self)
        caret = nil
        typedSelection = nil
        selected = range
        tintColor = .systemBlue
        inputDelegate?.selectionDidChange(self)
    }

    /// First responder without the keyboard, when it was not up.
    private func holdQuietly() {
        guard !isFirstResponder else { return }
        quiet = true
        if !super.becomeFirstResponder() { quiet = false }
        updateScreenWanted()
    }

    private func showPressMenu(at point: CGPoint) {
        guard window != nil else { return }
        pressMenu.presentEditMenu(with: UIEditMenuConfiguration(identifier: nil, sourcePoint: point))
    }

    /// The menu sits over the selection's first row, when there is one.
    func editMenuInteraction(_ interaction: UIEditMenuInteraction, targetRectFor configuration: UIEditMenuConfiguration) -> CGRect {
        if let selected, let first = selectionRects(for: Range(selected.a, selected.b)).first {
            return first.rect
        }
        return CGRect(origin: configuration.sourcePoint, size: .zero)
    }

    func editMenuInteraction(_ interaction: UIEditMenuInteraction, willPresentMenuFor configuration: UIEditMenuConfiguration, animator: any UIEditMenuInteractionAnimating) {
        pressMenuUp = true
    }

    func editMenuInteraction(_ interaction: UIEditMenuInteraction, willDismissMenuFor configuration: UIEditMenuConfiguration, animator: any UIEditMenuInteractionAnimating) {
        animator.addCompletion { [weak self] in
            guard let self else { return }
            self.pressMenuUp = false
            if self.menuAgain {
                self.menuAgain = false
                if self.selected != nil { self.showPressMenu(at: CGPoint(x: self.bounds.midX, y: self.bounds.midY)) }
            } else if self.quiet, self.selected == nil {
                // Held for the menu alone, and it is gone.
                self.resignFirstResponder()
            }
        }
    }

    /// A finger held down is selecting: it neither scrolls the terminal
    /// nor flicks to another tab.
    override func gestureRecognizerShouldBegin(_ g: UIGestureRecognizer) -> Bool {
        if pressing, g is UIPanGestureRecognizer { return false }
        // A finger resting on one of the selection's handles is about to
        // move it, not to select the word under it.
        if g === press, let selected {
            let rects = selectionRects(for: Range(selected.a, selected.b))
            if let first = rects.first?.rect, let last = rects.last?.rect {
                let at = g.location(in: self)
                let reach: CGFloat = 22
                let start = CGRect(x: first.minX, y: first.minY, width: 0, height: first.height).insetBy(dx: -reach, dy: -reach)
                let end = CGRect(x: last.maxX, y: last.minY, width: 0, height: last.height).insetBy(dx: -reach, dy: -reach)
                if start.contains(at) || end.contains(at) { return false }
            }
        }
        return super.gestureRecognizerShouldBegin(g)
    }

    override var inputView: UIView? { quiet ? noKeyboard : nil }

    @objc private func flicked(_ g: UIPanGestureRecognizer) {
        guard g.state == .ended else { return }
        let t = g.translation(in: self)
        let v = g.velocity(in: self)
        // One from the screen's left edge opens the sidebar instead, where
        // that edge does.
        let start = g.location(in: nil).x - g.translation(in: nil).x
        if model?.edgePullsDrawer == true, start < TerminalScreen.edgeWidth { return }
        // And one from the right edge, the right sidebar, wide or narrow:
        // the edge as `TerminalScreen.pullsRight` has it, in by the safe
        // area.
        if let window, start > window.bounds.width - window.safeAreaInsets.right - TerminalScreen.edgeWidth { return }
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
        // The system's long press over editable text places a caret under
        // a loupe and raises the keyboard. The view's own selects instead,
        // so the system's goes ahead only where this one has failed.
        a === press && b is UILongPressGestureRecognizer
    }

    override var canBecomeFirstResponder: Bool { true }

    @discardableResult
    override func becomeFirstResponder() -> Bool {
        // The system's own tap asks for the keyboard as ours does.
        if notATap, !isFirstResponder { return false }
        if quiet {
            // Held without a keyboard, and now asked for it.
            quiet = false
            if isFirstResponder {
                reloadInputViews()
                return true
            }
        }
        let became = super.becomeFirstResponder()
        updateScreenWanted()
        return became
    }

    @discardableResult
    override func resignFirstResponder() -> Bool {
        let resigned = super.resignFirstResponder()
        // The handles go with the keyboard, and so does what they held:
        // unseen, it would still hold off the edge's swipe.
        if resigned, selected != nil {
            selected = nil
            tintColor = .clear
            inputDelegate?.selectionDidChange(self)
        }
        // The next keyboard starts with nothing typed.
        if resigned { forgetTyped(keeping: 0) }
        if resigned { quiet = false }
        updateScreenWanted()
        return resigned
    }

    /// The text system reads the rows while the keyboard is up (marked
    /// text, the caret) or a selection is held: the core keeps them current
    /// then. Otherwise they are made when asked, at a wait.
    private func updateScreenWanted() {
        model?.setScreenWanted(isFirstResponder || selected != nil)
    }

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
        lineDisturbed()
    }

    // MARK: what the keyboard typed

    /// Something other than the keyboard typed (the key bar, a paste, a
    /// hardware key), or the line is another's now (a tap, a pane gone):
    /// what the keyboard typed stays readable, but it is no longer the
    /// end of the line, so none of it can be changed.
    func lineDisturbed() {
        guard !sending else { return }
        settled = typed.utf16.count
        typedSelection = nil
    }

    /// Sends the keyboard's text and keeps it. A line break is the Enter
    /// key, and enters everything before it.
    private func type(_ text: String) {
        guard let model, !text.isEmpty else { return }
        // A sticky Ctrl or Alt makes a chord of it: a key, not text. Return
        // is the Enter key with it (Alt+Enter, not Alt and a line feed).
        if model.ctrlSticky || model.altSticky {
            if text == "\n" {
                model.key("Enter")
            } else {
                model.text(text)
            }
            return
        }
        sending = true
        defer { sending = false }
        var entered = false
        let lines = text.split(separator: "\n", omittingEmptySubsequences: false)
        for (i, line) in lines.enumerated() {
            if i > 0 {
                model.key("Enter")
                entered = true
            }
            if !line.isEmpty { model.text(String(line)) }
        }
        typed += text
        if entered {
            settled = typed.utf16.count - (lines.last?.utf16.count ?? 0)
        }
        let length = typed.utf16.count
        if length > Self.typedKept * 4 || (entered && length > Self.typedKept) {
            forgetTyped(keeping: Self.typedKept)
        }
    }

    /// The place `offset` UTF-16 units into `typed`, when a character
    /// starts there or the text ends there.
    private func typedIndex(_ offset: Int) -> String.Index? {
        let units = typed.utf16
        guard offset >= 0, offset <= units.count else { return nil }
        let i = units.index(units.startIndex, offsetBy: offset)
        if i == typed.endIndex { return i }
        return typed.rangeOfComposedCharacterSequence(at: i).lowerBound == i ? i : nil
    }

    /// Takes `typed[a..<b]` (UTF-16 units) out of the line and puts
    /// `text` there: backspaces back to `a`, then `text` and whatever
    /// followed the stretch, typed again. False when the stretch is not
    /// the keyboard's to change, or cuts a character in two.
    private func retype(_ a: Int, _ b: Int, with text: String) -> Bool {
        guard let model, marked == nil, a >= settled, a <= b, !model.ctrlSticky, !model.altSticky,
              let from = typedIndex(a), let to = typedIndex(b) else { return false }
        let rest = String(typed[to...])
        sending = true
        // A backspace takes one character off the line, as the programs
        // a phone types into count them.
        for _ in 0..<typed[from...].count { model.key("Backspace") }
        sending = false
        typed.removeSubrange(from...)
        typedSelection = nil
        type(text + rest)
        return true
    }

    /// Drops what the keyboard typed, all but about the last `keeping`
    /// UTF-16 units. The document's tail moves, so the text system is told.
    private func forgetTyped(keeping: Int) {
        var cut = typed.utf16.count - keeping
        guard cut > 0 else { return }
        // Never through the middle of a character.
        while typedIndex(cut) == nil { cut += 1 }
        guard let from = typedIndex(cut) else { return }
        inputDelegate?.textWillChange(self)
        typed.removeSubrange(..<from)
        settled = max(settled - cut, 0)
        typedSelection = nil
        inputDelegate?.textDidChange(self)
    }

    // MARK: UIKeyInput

    var hasText: Bool { marked?.isEmpty == false || !typed.isEmpty }

    /// iOS turns two spaces after a word into a full stop and a space,
    /// now that it can read the word: it puts ". " where the first space
    /// was. A terminal's two spaces are two spaces.
    private static let periodShortcut = ". "

    func insertText(_ text: String) {
        // What the keyboard types goes at the end of the line, wherever a
        // tap left the caret, and the keyboard is told so: a dictation's
        // later corrections look for its words where the caret says.
        caret = nil
        endComposing()
        if let selection = typedSelection, retype(selection.a, selection.b, with: text) { return }
        typedSelection = nil
        if spaceTaken, text == Self.periodShortcut {
            spaceTaken = false
            type("  ")
            return
        }
        type(text)
    }

    func deleteBackward() {
        if marked != nil { return }
        caret = nil
        if let selection = typedSelection, retype(selection.a, selection.b, with: "") { return }
        typedSelection = nil
        sending = true
        model?.key("Backspace")
        sending = false
        // The character it took, if the keyboard typed it on this line.
        if typed.utf16.count > settled, !typed.isEmpty {
            if typed.removeLast() == " " {
                spaceTaken = true
                DispatchQueue.main.async { [weak self] in self?.spaceTaken = false }
            }
            settled = min(settled, typed.utf16.count)
        }
    }

    // MARK: the document

    /// The core published new rows. The text system is told only when
    /// the grid changed shape: under a program that redraws all the while
    /// (a spinner, a full-screen interface) the rows' own text never
    /// rests, and each telling reset the keyboard's candidates -- it
    /// flickered -- and dropped a selection as it was being made. A
    /// cell's index stands for the same place whatever is written in it,
    /// and what is copied is read when it is copied. While a composition
    /// is open nothing is told at all: the candidates would go.
    func screenChanged() {
        guard marked == nil, let before = screen else {
            // Nobody has read the rows since they last changed.
            screen = nil
            return
        }
        screen = nil
        guard let after = currentScreen() else {
            inputDelegate?.textWillChange(self)
            inputDelegate?.textDidChange(self)
            return
        }
        let reshaped = before.cols != after.cols || before.rows != after.rows
            || before.origin != after.origin || before.cell != after.cell
        if reshaped {
            screen = before
            inputDelegate?.textWillChange(self)
            screen = after
            inputDelegate?.textDidChange(self)
        }
    }

    /// A frame was painted while the rows are not kept current: the next
    /// question reads them again.
    func dropScreen() { screen = nil }

    /// The rows: the core's published ones while they are wanted, read
    /// without a wait; otherwise asked for once, waiting for the core.
    private func currentScreen() -> ScreenText? {
        if let screen { return screen }
        guard let model else { return nil }
        let json = model.screenWanted && model.screenJSON != "null" ? model.screenJSON : model.core.screenText()
        guard let s = ViewJSON.decode(ScreenText.self, json) else { return nil }
        screen = s
        return s
    }

    /// The rows' length in the document; what the keyboard typed and
    /// the marked text follow it, in that order.
    private var screenLength: Int {
        currentScreen().map { $0.rows * ($0.cols + 1) } ?? 0
    }

    private var typedLength: Int { typed.utf16.count }
    private var markedLength: Int { marked?.utf16.count ?? 0 }
    private var documentLength: Int { screenLength + typedLength + markedLength }

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

    // MARK: UITextInput -- marked text (after the rows and the typed text)

    var markedTextRange: UITextRange? {
        guard let marked, !marked.isEmpty else { return nil }
        let n = screenLength + typedLength
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
        // Composing over a selected stretch starts by taking it out.
        if marked == nil, let selection = typedSelection {
            _ = retype(selection.a, selection.b, with: "")
        }
        typedSelection = nil
        caret = nil
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
            type(marked)
            return
        }
        endComposing()
    }

    // MARK: UITextInput -- the selection

    /// The pane's selection while there is one, else the caret at the
    /// end of the marked text (the keyboard's insertion point).
    /// The system's selection over the rows, as document offsets. It is
    /// the shell's alone: the core is not told, so it draws no highlight
    /// of its own under the system's. Only the copy asks the core, for
    /// the selected cells' own text.
    private var selected: (a: Int, b: Int)? {
        didSet { updateScreenWanted() }
    }

    /// A selection is up, its handles to be moved.
    var holdsSelection: Bool { selected != nil }

    var selectedTextRange: UITextRange? {
        get {
            if marked == nil, let selected {
                return Range(selected.a, selected.b)
            }
            if marked == nil, let caret, caret <= screenLength {
                return Range(caret, caret)
            }
            if marked == nil, let typedSelection {
                return Range(screenLength + typedSelection.a, screenLength + typedSelection.b)
            }
            let n = documentLength
            return Range(n, n)
        }
        set {
            // The press has the selection while the finger is down.
            if pressing { return }
            defer { tintColor = selected == nil ? .clear : .systemBlue }
            typedSelection = nil
            guard let r = newValue as? Range else {
                caret = nil
                selected = nil
                return
            }
            // A stretch of what the keyboard typed: nothing to show, but
            // what it types next goes there. A caret in it is the line's
            // own, at the end.
            if r.a >= screenLength {
                caret = nil
                selected = nil
                let a = r.a - screenLength, b = min(r.b - screenLength, typedLength)
                if marked == nil, b > a, a >= settled { typedSelection = (a, b) }
                return
            }
            // A non-empty range inside the rows is a selection; a caret
            // inside them is remembered and clears it.
            if r.b > r.a, r.a < screenLength {
                caret = nil
                selected = (r.a, min(r.b, screenLength))
            } else {
                caret = r.a < screenLength ? r.a : nil
                selected = nil
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
        if r.b > n {
            let tail = typed + (marked ?? "")
            let m = tail.utf16
            let a = max(r.a - n, 0), b = min(r.b - n, m.count)
            if a < b {
                out += String(m[m.index(m.startIndex, offsetBy: a)..<m.index(m.startIndex, offsetBy: b)]) ?? ""
            }
        }
        return out
    }

    /// The keyboard puts other words where its own were: a dictation
    /// hearing them better, a correction. Only what it typed at the end
    /// of this line can be changed; anywhere else the text is typed in.
    func replace(_ range: UITextRange, withText text: String) {
        let n = screenLength
        if marked == nil, let r = range as? Range, r.a >= n, r.b > r.a {
            if text == Self.periodShortcut, r.b - r.a == 1, r.b - n == typedLength, typed.hasSuffix(" ") {
                insertText(" ")
                return
            }
            if retype(r.a - n, r.b - n, with: text) { return }
        }
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
        case #selector(copy(_:)): return selected != nil
        case #selector(paste(_:)): return UIPasteboard.general.hasStrings
        case #selector(selectAll(_:)): return currentScreen() != nil
        default: return false
        }
    }

    /// The selected cells' text, each row's trailing blanks dropped, as
    /// a terminal copies. The core reads it from the cells: the rows here
    /// put a stand-in after every wide character and in place of any
    /// character outside the BMP, and a copy must carry neither.
    func selectedText() -> String? {
        guard let selected, selected.b > selected.a, let s = currentScreen() else { return nil }
        // A selection that starts on a row's line break starts on the next row.
        let stride = s.cols + 1
        let start = selected.a % stride == s.cols ? selected.a + 1 : selected.a
        // What is still on the rows, if they shrank since it was selected.
        let end = min(selected.b, screenLength)
        guard start < end, let first = cell(at: start), let last = cell(at: end - 1),
              let text = model?.copyText(from: first, to: last), !text.isEmpty else { return nil }
        return text
    }

    override func copy(_ sender: Any?) {
        if let text = selectedText() {
            UIPasteboard.general.string = text
            model?.showCopied()
        }
        selected = nil
        tintColor = .clear
        inputDelegate?.selectionDidChange(self)
        // Held for the selection alone: with it gone, so is the hold.
        if quiet { resignFirstResponder() }
    }

    override func paste(_ sender: Any?) {
        model?.pasteFromClipboard()
    }

    override func selectAll(_ sender: Any?) {
        guard let s = currentScreen() else { return }
        holdQuietly()
        select((0, index(row: s.rows - 1, col: s.cols - 1) + 1))
        menuAgain = pressMenuUp
    }

    // MARK: traits: a terminal wants raw keys, no autocorrect

    // Stored, not computed: the traits protocol declares them settable,
    // and UIKit reads them through the setter's counterpart. The inline
    // prediction is the one that draws the suggestion bar on iOS 17+.
    // The keyboard is whichever the user has: an ASCII-only one hides the
    // Chinese and Japanese keyboards the marked text below is for.
    var keyboardType: UIKeyboardType = .default
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

/// Watches the touches without ever recognizing anything: how far the
/// first finger has gone from where it landed. Every recognizer is handed
/// a touch before any of them acts on it, so this is known by the time a
/// tap is told.
final class TravelWatch: UIGestureRecognizer {
    private var finger: UITouch?
    private var start = CGPoint.zero
    private(set) var farthest: CGFloat = 0
    var began: (() -> Void)?
    var over: (() -> Void)?
    var tracking: Bool { finger != nil }

    override init(target: Any?, action: Selector?) {
        super.init(target: target, action: action)
        cancelsTouchesInView = false
        delaysTouchesEnded = false
    }

    func forget() { farthest = 0 }

    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent) {
        guard finger == nil, let touch = touches.first else { return }
        finger = touch
        start = touch.location(in: nil)
        farthest = 0
        began?()
    }

    override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent) {
        measure(touches)
    }

    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent) {
        measure(touches)
        finish(touches)
    }

    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent) {
        finish(touches)
    }

    private func measure(_ touches: Set<UITouch>) {
        guard let finger, touches.contains(finger) else { return }
        let at = finger.location(in: nil)
        farthest = max(farthest, hypot(at.x - start.x, at.y - start.y))
    }

    private func finish(_ touches: Set<UITouch>) {
        guard let finger, touches.contains(finger) else { return }
        self.finger = nil
        state = .failed
        over?()
    }

    override func reset() {
        finger = nil
    }

    override func canPrevent(_ preventedGestureRecognizer: UIGestureRecognizer) -> Bool { false }
    override func canBePrevented(by preventingGestureRecognizer: UIGestureRecognizer) -> Bool { false }
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
