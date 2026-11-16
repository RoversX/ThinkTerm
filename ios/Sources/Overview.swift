import SwiftUI
import CoreText

/// The desktop's Live Overview: a card for each thread running in the
/// Space on show, as many to a row as the width takes, each in the
/// terminal's own shape with its whole screen in it -- the server's rows,
/// in the terminal's face and colours, fetched again while the overview
/// is up. The thread on show is the terminal itself on the way in and out
/// (`TerminalScreen`): its card's picture shows once the terminal has
/// faded into it.
struct OverviewScreen: View {
    @ObservedObject var model: TerminalModel
    @ObservedObject private var lang = AppLanguage.shared
    @Environment(\.scenePhase) private var phase
    /// The terminal's size: every picture has its shape.
    var stage: CGSize
    var liveThread: String?
    /// The live card's picture shows: the terminal has faded into it.
    var liveShown: Bool
    /// Where a card's picture is, in the screen's space, as it moves.
    var onThumb: (String, CGRect) -> Void
    var onPick: (ThreadView) -> Void
    var onClose: () -> Void
    @State private var sight = Sight()

    /// The cards in sight, once any has said: only their screens are
    /// fetched again. Not the view's state: a card scrolled in or out
    /// changes nothing on show.
    private final class Sight {
        var shown: Set<String>?
    }

    static let gap: CGFloat = 12
    private static let margin: CGFloat = 14
    /// Above the heading, under it, and round a card's picture.
    private static let top: CGFloat = 4
    private static let headingGap: CGFloat = 10
    private static let inset: CGFloat = 6
    /// The picture's corners; the zoom rounds the terminal to them.
    static let thumbRadius: CGFloat = 10
    /// Behind the cards, and the bar over them.
    static let backdrop = Color(white: 0.06)

    /// The terminal's shape, a phone's before it is measured.
    private var aspect: CGFloat {
        stage.width > 0 && stage.height > 0 ? stage.width / stage.height : 0.6
    }

    /// Running threads only, as on the desktop: a thread that runs nothing
    /// has no screen to show.
    private var threads: [ThreadView] { (model.threads?.threads ?? []).filter(\.live) }

    /// A card's width: at least this, at most that, as on the desktop.
    private static let minCard: CGFloat = 160
    private static let maxCard: CGFloat = 640
    /// Below this, a column goes sooner than a lone card in the last row.
    private static let comfortCard: CGFloat = 240

    /// The desktop's columns (`shared_grid_columns`): as many as fit at the
    /// least width, up to four, never more than there are cards; one fewer
    /// when that leaves no lone card under a full row and the cards are
    /// narrow anyway.
    static func columns(for width: CGFloat, count: Int) -> Int {
        var columns = max(1, min(4, Int((width + gap) / (minCard + gap)), count))
        let lone = { (columns: Int) in count > columns && count % columns == 1 }
        if columns > 2, cardWidth(for: width, columns: columns) < comfortCard, lone(columns), !lone(columns - 1) {
            columns -= 1
        }
        return columns
    }

    /// Never below a point: a first pass may offer no width at all.
    static func cardWidth(for width: CGFloat, columns: Int) -> CGFloat {
        max(1, min(maxCard, (width - gap * CGFloat(columns - 1)) / CGFloat(columns)))
    }

    /// Where the picture of the card at `index` of `count` will be, `width`
    /// across, before the cards are laid out: the terminal's flight sets off
    /// for it at once, and turns to where the card turns out to be.
    static func estimatedThumb(index: Int, count: Int, width: CGFloat, aspect: CGFloat) -> CGRect {
        let inner = width - 2 * margin
        let columns = columns(for: inner, count: count)
        let cardWidth = cardWidth(for: inner, columns: columns)
        let (row, column) = (index / columns, index % columns)
        let inRow = min(columns, count - row * columns)
        let rowWidth = CGFloat(inRow) * cardWidth + gap * CGFloat(inRow - 1)
        let thumbWidth = max(1, cardWidth - 2 * inset)
        let thumbHeight = thumbWidth / max(aspect, 0.1)
        let heading = UIFont.preferredFont(forTextStyle: .footnote).lineHeight
        let cardHeading = UIFont.preferredFont(forTextStyle: .subheadline).lineHeight + CardHeading.padding
        let cardHeight = cardHeading + thumbHeight + inset
        return CGRect(
            x: margin + (inner - rowWidth) / 2 + CGFloat(column) * (cardWidth + gap) + inset,
            y: top + heading + headingGap + CGFloat(row) * (cardHeight + gap) + cardHeading,
            width: thumbWidth,
            height: thumbHeight
        )
    }

    var body: some View {
        GeometryReader { geo in
            let inner = geo.size.width - 2 * Self.margin
            let columns = Self.columns(for: inner, count: threads.count)
            ScrollViewReader { reader in
                ScrollView {
                    VStack(alignment: .leading, spacing: Self.headingGap) {
                        heading
                        if threads.isEmpty {
                            Text(tr("overview.empty"))
                                .font(.callout)
                                .foregroundStyle(.secondary)
                                .frame(maxWidth: .infinity)
                                .padding(.top, 60)
                        } else {
                            grid(columns: columns, cardWidth: Self.cardWidth(for: inner, columns: columns))
                        }
                    }
                    .padding(.horizontal, Self.margin)
                    .padding(.top, Self.top)
                    .padding(.bottom, 24 + TerminalScreen.bottomInset)
                }
                .onAppear {
                    // The thread on show in sight before the terminal goes
                    // into its card.
                    if let live = liveThread { reader.scrollTo(live, anchor: .center) }
                }
            }
        }
        .foregroundColor(.white)
        .dynamicTypeSize(...DynamicTypeSize.xxLarge)
        // Pinching back in returns to the terminal.
        .simultaneousGesture(MagnifyGesture().onEnded { value in
            if value.magnification > 1.25 { onClose() }
        })
        .task(id: phase) {
            // One ticker while the overview is up and the app in front.
            guard phase == .active else { return }
            while !Task.isCancelled {
                for thread in threads where sight.shown?.contains(thread.id) ?? true {
                    if let pane = Self.pane(of: thread) { model.requestScreen(pane: pane) }
                }
                try? await Task.sleep(for: .milliseconds(1500))
            }
        }
    }

    /// The Space and how many of its threads run.
    private var heading: some View {
        HStack(spacing: 8) {
            Text(model.threads?.space ?? "").font(.footnote.weight(.semibold))
            Rectangle().fill(Color.white.opacity(0.1)).frame(height: 1)
            Text(verbatim: "\(threads.count)").font(.footnote.monospacedDigit())
        }
        .foregroundStyle(.secondary)
    }

    /// Rows of cards of one width, each row centred as the desktop's are.
    private func grid(columns: Int, cardWidth: CGFloat) -> some View {
        VStack(spacing: Self.gap) {
            ForEach(Array(stride(from: 0, to: threads.count, by: columns)), id: \.self) { start in
                HStack(alignment: .top, spacing: Self.gap) {
                    ForEach(threads[start..<min(start + columns, threads.count)]) { card($0).frame(width: cardWidth) }
                }
                .frame(maxWidth: .infinity)
            }
        }
    }

    /// The pane a thread's card shows: its current tab's.
    static func pane(of thread: ThreadView) -> Int? {
        (thread.tabs.first(where: \.current) ?? thread.tabs.first)?.target
    }

    private func card(_ thread: ThreadView) -> some View {
        let live = thread.id == liveThread
        let pane = Self.pane(of: thread)
        let screen = pane.flatMap { model.screens[$0] }
        return Button { onPick(thread) } label: {
            VStack(alignment: .leading, spacing: 0) {
                CardHeading(thread: thread)
                ScreenThumb(model: model, pane: pane, picture: pane.flatMap { model.pictures[$0] }, serial: pane.flatMap { model.screenSerials[$0] } ?? 0)
                    .opacity(live && !liveShown ? 0 : 1)
                    // The terminal's own background under its picture, so a
                    // fade between the two never shows the card through.
                    .background(live ? model.background : (screen.flatMap { Color(hex: $0.background) } ?? model.background))
                    .aspectRatio(aspect, contentMode: .fit)
                    .clipShape(RoundedRectangle(cornerRadius: Self.thumbRadius, style: .continuous))
                    .onGeometryChange(for: CGRect.self) { $0.frame(in: .named("screen")) } action: { onThumb(thread.id, $0) }
                    .padding([.horizontal, .bottom], Self.inset)
            }
            .background(Color(white: 0.13), in: RoundedRectangle(cornerRadius: 16, style: .continuous))
            .overlay {
                if live {
                    RoundedRectangle(cornerRadius: 16, style: .continuous).strokeBorder(Color.accentColor, lineWidth: 2)
                }
            }
            .contentShape(RoundedRectangle(cornerRadius: 16, style: .continuous))
        }
        .buttonStyle(.plain)
        .contextMenu { AppMenuItems(model: model, kind: "thread", id: thread.id) }
        .onScrollVisibilityChange(threshold: 0.05) { visible in
            var shown = sight.shown ?? []
            if visible {
                shown.insert(thread.id)
                // Scrolled into sight: its screen as it is now, not at the
                // next tick.
                if let pane = Self.pane(of: thread) { model.requestScreen(pane: pane) }
            } else {
                shown.remove(thread.id)
            }
            sight.shown = shown
        }
        .id(thread.id)
    }
}

/// A card's top: the thread's mark, its project and name as the desktop
/// titles a card, and what its current tab runs.
private struct CardHeading: View {
    let thread: ThreadView
    /// Above and below the line.
    static let padding: CGFloat = 16

    private var title: String {
        thread.project.isEmpty || thread.project == thread.name ? thread.name : "\(thread.project) · \(thread.name)"
    }

    var body: some View {
        let running = (thread.tabs.first(where: \.current) ?? thread.tabs.first)?.title ?? ""
        HStack(spacing: 6) {
            ThreadMark(status: thread.markStatus, dot: thread.dot, size: 7)
            // The title keeps its room; what runs gives way first.
            Text(title).font(.subheadline.weight(.semibold)).lineLimit(1).layoutPriority(1)
            Spacer(minLength: 4)
            Text(running).font(.caption2.monospaced()).foregroundStyle(.secondary).lineLimit(1)
        }
        .padding(.horizontal, 10)
        .padding(.top, 9)
        .padding(.bottom, Self.padding - 9)
    }
}

/// A pane's whole screen in miniature: the picture `ScreenPainter` drew
/// of it at the box's size, drawn again when the screen or the box
/// changes. Its picture and the screen's serial come from the card, so a
/// card is looked at again only when they or the box change.
struct ScreenThumb: View {
    let model: TerminalModel
    let pane: Int?
    let picture: CGImage?
    let serial: Int
    @Environment(\.displayScale) private var scale

    private struct Turn: Equatable {
        var pane: Int?
        var serial: Int
        var size: CGSize
    }

    var body: some View {
        GeometryReader { geo in
            let turn = Turn(pane: pane, serial: serial, size: geo.size)
            ZStack(alignment: .topLeading) {
                if let picture {
                    Image(decorative: picture, scale: scale)
                        .resizable()
                        .frame(width: geo.size.width, height: geo.size.height)
                }
            }
            .task(id: turn) {
                guard let pane = turn.pane, turn.serial > 0, turn.size.width > 0 else { return }
                model.paintScreen(pane: pane, pixels: CGSize(width: turn.size.width * scale, height: turn.size.height * scale))
            }
        }
        .allowsHitTesting(false)
    }
}

/// Draws a tab as a picture, split as it is: every pane's runs at their
/// own columns where the pane lies, in the terminal's face and colours,
/// the cells as large as the picture lets the tab's columns and rows fit,
/// from the top left. Runs off the main thread (`TerminalModel.paintScreen`).
enum ScreenPainter {
    static func paint(_ tab: TabPreview, pixels: CGSize, face: String) -> CGImage? {
        let width = Int(pixels.width.rounded()), height = Int(pixels.height.rounded())
        guard width > 0, height > 0, tab.cols > 0, tab.rows > 0,
              let space = CGColorSpace(name: CGColorSpace.sRGB),
              let context = CGContext(
                  data: nil, width: width, height: height, bitsPerComponent: 8, bytesPerRow: 0, space: space,
                  bitmapInfo: CGImageAlphaInfo.premultipliedFirst.rawValue | CGBitmapInfo.byteOrder32Little.rawValue
              ) else { return nil }
        context.setFillColor(color(tab.background) ?? CGColor(gray: 0, alpha: 1))
        context.fill(CGRect(x: 0, y: 0, width: width, height: height))

        let unit = TerminalFaces.font(face, size: 100)
        let advance = max(Self.advance(of: unit) / 100, 0.3)
        let line = max((CTFontGetAscent(unit) + CTFontGetDescent(unit) + CTFontGetLeading(unit)) / 100, 1)
        // A cell's width: the tab's columns across, or its rows down,
        // whichever runs out first.
        let cell = min(CGFloat(width) / CGFloat(tab.cols), CGFloat(height) / CGFloat(tab.rows) / line * advance)
        let font = TerminalFaces.font(face, size: cell / advance)
        let rowHeight = cell / advance * line
        let ascent = CTFontGetAscent(font)
        // The rows go down from the top; CoreGraphics counts up.
        let top = { (row: Int) in CGFloat(height) - CGFloat(row) * rowHeight }

        for pane in tab.panes {
            let screen = pane.screen
            let x0 = CGFloat(pane.left) * cell
            let area = CGRect(x: x0, y: top(pane.top + screen.rows), width: CGFloat(screen.cols) * cell, height: CGFloat(screen.rows) * rowHeight)
            // Nothing of a pane past its own place: a line longer than it,
            // or one wider since its place was listed, stops at its edge.
            context.saveGState()
            defer { context.restoreGState() }
            context.clip(to: area)
            if screen.background != tab.background, let back = color(screen.background) {
                context.setFillColor(back)
                context.fill(area)
            }
            let foreground = color(screen.foreground) ?? CGColor(gray: 1, alpha: 1)
            for (row, runs) in screen.lines.enumerated() {
                let y = top(pane.top + row)
                for run in runs {
                    let x = x0 + CGFloat(run.col) * cell
                    if let back = color(run.bg) {
                        context.setFillColor(back)
                        context.fill(CGRect(x: x, y: y - rowHeight, width: CGFloat(run.width) * cell, height: rowHeight))
                    }
                    guard !run.text.allSatisfy({ $0 == " " }) else { continue }
                    draw(run, font: font, color: color(run.fg) ?? foreground, at: CGPoint(x: x, y: y - ascent), cell: cell, in: context)
                }
            }
        }
        dividers(of: tab, cell: cell, rowHeight: rowHeight, top: top, in: context)
        return context.makeImage()
    }

    /// The splits: a hairline down or across the middle of the cells
    /// between one pane and the next, as the terminal draws them.
    private static func dividers(of tab: TabPreview, cell: CGFloat, rowHeight: CGFloat, top: (Int) -> CGFloat, in context: CGContext) {
        guard tab.panes.count > 1 else { return }
        context.setFillColor((color(tab.foreground) ?? CGColor(gray: 1, alpha: 1)).copy(alpha: 0.3) ?? CGColor(gray: 0.5, alpha: 1))
        let thickness = max(1, cell / 8)
        for pane in tab.panes {
            let (cols, rows) = (pane.screen.cols, pane.screen.rows)
            if pane.left + cols < tab.cols {
                let x = (CGFloat(pane.left + cols) + 0.5) * cell - thickness / 2
                context.fill(CGRect(x: x, y: top(pane.top + rows), width: thickness, height: CGFloat(rows) * rowHeight))
            }
            if pane.top + rows < tab.rows {
                let y = top(pane.top + rows) - rowHeight / 2 - thickness / 2
                context.fill(CGRect(x: CGFloat(pane.left) * cell, y: y, width: CGFloat(cols) * cell, height: thickness))
            }
        }
    }

    /// A run from its baseline at `origin`: as one line when it is letters
    /// of the face, each a cell wide, or one wide character; else each
    /// character at its own cell, so one another face draws (an icon, a
    /// box's line) at its own width moves none of the rest off its column.
    private static func draw(_ run: ScreenRun, font: CTFont, color: CGColor, at origin: CGPoint, cell: CGFloat, in context: CGContext) {
        let text = NSAttributedString(string: run.text, attributes: [
            NSAttributedString.Key(kCTFontAttributeName as String): font,
            NSAttributedString.Key(kCTForegroundColorAttributeName as String): color,
        ])
        let line = CTLineCreateWithAttributedString(text)
        // A character as the terminal has one, a cell's: a letter and its
        // marks, a flag's two halves.
        guard !run.text.allSatisfy(\.isASCII), run.text.count == run.width else {
            context.textPosition = origin
            CTLineDraw(line, context)
            return
        }
        // The cell of each UTF-16 offset in the text: a character a cell.
        var cells: [Int] = []
        for (index, character) in run.text.enumerated() {
            cells.append(contentsOf: repeatElement(index, count: character.utf16.count))
        }
        // Where the line put each cell's first glyph: its marks keep their
        // places beside it.
        var firsts: [Int: CGFloat] = [:]
        context.setFillColor(color)
        // The glyphs' places are the context's own: none of the text
        // position a line drawn before left behind.
        context.textPosition = .zero
        for glyphs in CTLineGetGlyphRuns(line) as? [CTRun] ?? [] {
            let count = CTRunGetGlyphCount(glyphs)
            guard count > 0 else { continue }
            var ids = [CGGlyph](repeating: 0, count: count)
            var offsets = [CFIndex](repeating: 0, count: count)
            var places = [CGPoint](repeating: .zero, count: count)
            CTRunGetGlyphs(glyphs, CFRange(location: 0, length: count), &ids)
            CTRunGetStringIndices(glyphs, CFRange(location: 0, length: count), &offsets)
            CTRunGetPositions(glyphs, CFRange(location: 0, length: count), &places)
            let positions = zip(offsets, places).map { offset, place in
                let at = cells[min(max(offset, 0), cells.count - 1)]
                let first = firsts[at] ?? place.x
                firsts[at] = first
                return CGPoint(x: origin.x + CGFloat(at) * cell + place.x - first, y: origin.y + place.y)
            }
            // The face the line fell back to for these, if not the terminal's.
            let attributes = CTRunGetAttributes(glyphs) as NSDictionary
            let face = attributes[kCTFontAttributeName as String].map { $0 as! CTFont } ?? font
            CTFontDrawGlyphs(face, ids, positions, count, context)
        }
    }

    /// The face's width for one cell, at its size.
    private static func advance(of font: CTFont) -> CGFloat {
        var glyph = CGGlyph()
        var character = UniChar(("M" as Unicode.Scalar).value)
        guard CTFontGetGlyphsForCharacters(font, &character, &glyph, 1) else { return CTFontGetSize(font) * 0.6 }
        return CTFontGetAdvancesForGlyphs(font, .horizontal, &glyph, nil, 1)
    }

    /// "#rrggbb" as a colour; anything else, none.
    private static func color(_ hex: String) -> CGColor? {
        guard hex.count == 7, hex.hasPrefix("#"), let value = UInt32(hex.dropFirst(), radix: 16) else { return nil }
        return CGColor(
            srgbRed: CGFloat((value >> 16) & 0xff) / 255,
            green: CGFloat((value >> 8) & 0xff) / 255,
            blue: CGFloat(value & 0xff) / 255,
            alpha: 1
        )
    }
}

/// The terminal's faces, which the core reads from the app's files, made
/// known to CoreText too, for the pictures of screens; the icons' face
/// after either, as the core falls back to it.
enum TerminalFaces {
    /// The system's faces with plain forms of the symbols that are text
    /// unless asked to be pictures, in the order tried: what the app's own
    /// faces and Menlo lack of them, the system's cascade hands to the
    /// emoji font. Of the 207 such characters these and Menlo have all
    /// that any plain face on an iPhone has; the 91 left are pictures
    /// wherever they are drawn (a thermometer, a cloud).
    static let plainFaces = ["STIXTwoMath-Regular", "AppleSymbols", "ZapfDingbatsITC", "STIXGeneral-Regular", "HiraMinProN-W3"]

    private static let plain: [CTFontDescriptor] = plainFaces.map {
        CTFontDescriptorCreateWithNameAndSize($0 as CFString, 0)
    }

    private static let symbols: [CTFontDescriptor] = {
        for name in ["JetBrainsMono-Regular", "FiraCode-Regular", "SymbolsNerdFontMono-Regular"] {
            if let url = Bundle.main.url(forResource: name, withExtension: "ttf") {
                CTFontManagerRegisterFontsForURL(url as CFURL, .process, nil)
            }
        }
        guard let url = Bundle.main.url(forResource: "SymbolsNerdFontMono-Regular", withExtension: "ttf") else { return [] }
        return CTFontManagerCreateFontDescriptorsFromURL(url as CFURL) as? [CTFontDescriptor] ?? []
    }()

    /// `face`, by its PostScript name, at `size`; what it lacks is looked
    /// for in the icons' face, then in the plain faces, before the
    /// system's -- so a screen's picture draws a symbol as the screen does.
    static func font(_ face: String, size: CGFloat) -> CTFont {
        let descriptor = CTFontDescriptorCreateWithAttributes([
            kCTFontNameAttribute: face,
            kCTFontCascadeListAttribute: symbols + plain,
        ] as CFDictionary)
        return CTFontCreateWithFontDescriptor(descriptor, size, nil)
    }
}

/// A picture of the terminal flown between the screen and a card by Core
/// Animation, which the render server runs: a main thread busy making or
/// laying out the cards does not stop it. It flies over the screen's
/// content (`Flyway`), laid out where the terminal is and moved by its
/// transform, over a cover that hides the content while the cards come
/// and go under it.
final class Flight {
    fileprivate weak var stage: UIView?
    private var picture: UIView?
    private var cover: UIView?
    private var frame: CGRect = .zero
    /// Counts the moves: the end of one overtaken is not this one's.
    private var turn = 0

    var flying: Bool { picture != nil }

    /// What the screen shows over `rect` (in the stage's space) now, as a
    /// view: the terminal as drawn, before it is hidden.
    func snapshot(_ rect: CGRect) -> UIView? {
        guard let stage, let window = stage.window, rect.width > 0, rect.height > 0 else { return nil }
        return window.resizableSnapshotView(from: stage.convert(rect, to: window), afterScreenUpdates: false, withCapInsets: .zero)
    }

    /// The screen's content under `color`, at `alpha`.
    func cover(_ color: UIColor, alpha: CGFloat) {
        guard let stage else { return }
        let view: UIView
        if let cover {
            view = cover
        } else {
            view = UIView(frame: stage.bounds)
            view.autoresizingMask = [.flexibleWidth, .flexibleHeight]
            view.isUserInteractionEnabled = false
            stage.insertSubview(view, at: 0)
            cover = view
        }
        view.backgroundColor = color
        view.alpha = alpha
    }

    func fadeCover(to alpha: CGFloat, duration: TimeInterval) {
        guard let cover else { return }
        UIView.animate(withDuration: duration, delay: 0, options: [.beginFromCurrentState, .curveEaseOut]) {
            cover.alpha = alpha
        }
    }

    /// `view`, laid out over `frame`, shown over `rect` straight away, over
    /// the cover.
    func begin(with view: UIView, frame: CGRect, at rect: CGRect, radius: CGFloat) {
        turn += 1
        picture?.removeFromSuperview()
        guard let stage else { return }
        view.frame = frame
        view.isUserInteractionEnabled = false
        view.layer.masksToBounds = true
        view.layer.cornerCurve = .continuous
        self.frame = frame
        place(view, at: rect, radius: radius)
        stage.addSubview(view)
        picture = view
    }

    /// To `rect`, rounded to `radius` there, in a spring; `done` once it
    /// is there and no later move has taken over.
    func move(to rect: CGRect, radius: CGFloat, duration: TimeInterval, done: @escaping () -> Void) {
        guard let picture else { return done() }
        turn += 1
        let mine = turn
        UIView.animate(springDuration: duration, bounce: 0, options: [.beginFromCurrentState]) {
            self.place(picture, at: rect, radius: radius)
        } completion: { _ in
            if mine == self.turn { done() }
        }
    }

    /// The picture and the cover gone in a fade, showing what is under them.
    func fadeOut(duration: TimeInterval, done: @escaping () -> Void) {
        guard picture != nil || cover != nil else { return done() }
        turn += 1
        let mine = turn
        let (picture, cover) = (self.picture, self.cover)
        UIView.animate(withDuration: duration, delay: 0, options: [.beginFromCurrentState, .curveEaseOut]) {
            picture?.alpha = 0
            cover?.alpha = 0
        } completion: { _ in
            guard mine == self.turn else { return }
            self.end()
            done()
        }
    }

    func end() {
        turn += 1
        picture?.removeFromSuperview()
        cover?.removeFromSuperview()
        picture = nil
        cover = nil
    }

    /// Scaled about its middle from `frame` to `rect`'s width and moved
    /// onto it; the corners' radius in the view's own points.
    private func place(_ view: UIView, at rect: CGRect, radius: CGFloat) {
        let scale = rect.width / max(frame.width, 1)
        view.transform = CGAffineTransform(translationX: rect.midX - frame.midX, y: rect.midY - frame.midY)
            .scaledBy(x: scale, y: scale)
        view.layer.cornerRadius = radius / max(scale, 0.01)
    }
}

/// Where a `Flight` flies: a clear view over the screen's content, every
/// touch going through it.
struct Flyway: UIViewRepresentable {
    let flight: Flight

    func makeUIView(context: Context) -> UIView {
        let view = PassThrough()
        view.backgroundColor = .clear
        flight.stage = view
        return view
    }

    func updateUIView(_ view: UIView, context: Context) {
        flight.stage = view
    }

    private final class PassThrough: UIView {
        override func hitTest(_ point: CGPoint, with event: UIEvent?) -> UIView? { nil }
    }
}
