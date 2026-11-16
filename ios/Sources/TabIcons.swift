import SwiftUI
import UIKit

/// The card heading a tab or a pane's capsule, as the desktop draws it: a
/// lit circle carrying the mark of what the pane runs. The core draws it
/// (`Core.cardsPicture`, from the look every client shares); this shows
/// the picture. A busy pane's mark gives way to the spinner, which shows
/// alone where there is no card (tab icons off, or a card from before the
/// host's came).
struct TabIconCard: View {
    let core: Core
    var id: String?
    var size: CGFloat
    var busy = false
    /// The cards' revision (`TerminalModel.iconsRevision`): a new one
    /// draws the card again.
    var revision: UInt64 = 0
    @Environment(\.displayScale) private var scale
    @Environment(\.colorScheme) private var scheme

    var body: some View {
        let images = TabIconImages.shared
        if let id, let cards = images.cards([id], points: size, scale: scale, dark: scheme == .dark, glyphs: !busy, core: core) {
            ZStack {
                CardsImage(cards: cards)
                if busy {
                    Spinner(color: images.icon(id, points: size, scale: scale, core: core)?.glyph ?? .white)
                        .padding(size * 0.22)
                }
            }
            .frame(width: size, height: size)
            .accessibilityHidden(true)
        } else if busy {
            Spinner(color: .white.opacity(0.7))
                .frame(width: size * 0.7, height: size * 0.7)
                .frame(width: size, height: size)
                .accessibilityHidden(true)
        }
    }
}

/// What a thread runs, leading its row as the desktop's does: the first
/// program's card, the second's small at its corner -- or, with more, how
/// many more. One picture from the core, in the room every row keeps for
/// its icons; only the number is written here, in the system's figures.
struct ThreadCards: View {
    let core: Core
    var icons: [String]
    var revision: UInt64
    var size: CGFloat = 30
    @Environment(\.displayScale) private var scale
    @Environment(\.colorScheme) private var scheme

    var body: some View {
        if let cards = TabIconImages.shared.cards(icons, points: size, scale: scale, dark: scheme == .dark, glyphs: true, core: core) {
            ZStack(alignment: .topLeading) {
                CardsImage(cards: cards)
                if let count = cards.count {
                    Text("\(min(count.more, 99))")
                        .font(.system(size: count.rect.width * 0.65, weight: .bold))
                        .foregroundColor(.white)
                        .frame(width: count.rect.width, height: count.rect.height)
                        .offset(x: count.rect.minX - cards.inset, y: count.rect.minY - cards.inset)
                }
            }
            .frame(width: cards.slot, height: size, alignment: .topLeading)
            .accessibilityHidden(true)
        }
    }
}

/// A picture of cards at its own size, its icons' box at the corner of
/// what lays it out: the shadows fall outside, as the desktop's do.
private struct CardsImage: View {
    let cards: TabIconImages.Cards

    var body: some View {
        Color.clear
            .overlay(alignment: .topLeading) {
                Image(uiImage: cards.image)
                    .fixedSize()
                    .offset(x: -cards.inset, y: -cards.inset)
            }
    }
}

/// The cards drawn so far, by id and pixel size: each is drawn once and
/// kept while the cards stay the same -- a few dozen built-in ones and at
/// most 64 of a host's own, at the one or two sizes the app uses.
final class TabIconImages {
    static let shared = TabIconImages()

    struct Icon {
        let circle: Color
        let glyph: Color
        /// The glyph's shape, white at its coverage; nil for a card without.
        let mask: UIImage?
    }

    /// A row's icons as the core drew them (`Core.cardsPicture`), in points.
    struct Cards {
        let image: UIImage
        /// How far into the picture the icons' own box starts.
        let inset: CGFloat
        /// The box's width: the room a row keeps for its icons.
        let slot: CGFloat
        /// The disc for the programs past the first, in the picture.
        let count: (more: Int, rect: CGRect)?
    }

    private var cache: [String: Icon] = [:]
    /// The pictures drawn so far, by what they show; nil for cards there
    /// are none of. A thread's programs come and go, so it is emptied when
    /// it reaches `pictureLimit` as well as with the cards.
    private var pictures: [String: Cards?] = [:]
    private static let pictureLimit = 256
    /// Ids the cards have none for, so as not to ask again.
    private var missing: Set<String> = []
    /// The core's revision of the cards in `cache`.
    private var revision: UInt64 = 0

    private init() {
        // Drawn again when next shown: let the memory go when it is short.
        NotificationCenter.default.addObserver(
            forName: UIApplication.didReceiveMemoryWarningNotification, object: nil, queue: .main
        ) { [weak self] _ in
            self?.cache.removeAll()
            self?.missing.removeAll()
            self?.pictures.removeAll()
        }
    }

    func cards(_ ids: [String], points: CGFloat, scale: CGFloat, dark: Bool, glyphs: Bool, core: Core) -> Cards? {
        guard !ids.isEmpty else { return nil }
        refresh(core)
        let pixels = UInt32((points * scale).rounded())
        let key = "\(ids.joined(separator: ","))@\(pixels)\(dark ? "d" : "l")\(glyphs ? "g" : "")"
        if let known = pictures[key] { return known }
        if pictures.count >= Self.pictureLimit { pictures.removeAll() }
        var cards: Cards?
        if let drawn = core.cardsPicture(ids: ids, diameter: pixels, scale: Float(scale), dark: dark, glyphs: glyphs),
           let image = Self.image(drawn.picture, scale: scale) {
            cards = Cards(
                image: image,
                inset: CGFloat(drawn.inset) / scale,
                slot: CGFloat(drawn.slot) / scale,
                count: drawn.count.map { count in
                    let side = CGFloat(count.diameter) / scale
                    return (Int(count.more), CGRect(x: CGFloat(count.x) / scale, y: CGFloat(count.y) / scale, width: side, height: side))
                }
            )
        }
        pictures[key] = cards
        return cards
    }

    /// Another host's cards, or the built-in ones again: what was drawn
    /// before is stale.
    private func refresh(_ core: Core) {
        let now = core.tabIconsRevision()
        if now != revision {
            cache.removeAll()
            missing.removeAll()
            pictures.removeAll()
            revision = now
        }
    }

    func icon(_ id: String, points: CGFloat, scale: CGFloat, core: Core) -> Icon? {
        refresh(core)
        let pixels = UInt32((points * 0.56 * scale).rounded(.up))
        let key = "\(id)@\(pixels)"
        if let icon = cache[key] { return icon }
        if missing.contains(key) { return nil }
        guard let card = core.tabIcon(id: id, size: pixels) else {
            missing.insert(key)
            return nil
        }
        let icon = Icon(
            circle: Color(hex: card.circle) ?? .gray,
            glyph: Color(hex: card.glyph) ?? .white,
            mask: Self.image(card, scale: scale)
        )
        cache[key] = icon
        return icon
    }

    /// The core's coverage bytes as a white image with that alpha, which a
    /// template image is tinted through.
    private static func image(_ card: TabIcon, scale: CGFloat) -> UIImage? {
        let size = Int(card.size)
        guard size > 0, card.mask.count == size * size else { return nil }
        var rgba = [UInt8](repeating: 0, count: size * size * 4)
        for (i, alpha) in card.mask.enumerated() {
            // Premultiplied white: every channel at the coverage.
            rgba[i * 4] = alpha
            rgba[i * 4 + 1] = alpha
            rgba[i * 4 + 2] = alpha
            rgba[i * 4 + 3] = alpha
        }
        guard let provider = CGDataProvider(data: Data(rgba) as CFData),
              let image = CGImage(
                width: size, height: size, bitsPerComponent: 8, bitsPerPixel: 32, bytesPerRow: size * 4,
                space: CGColorSpaceCreateDeviceRGB(),
                bitmapInfo: CGBitmapInfo(rawValue: CGImageAlphaInfo.premultipliedLast.rawValue),
                provider: provider, decode: nil, shouldInterpolate: true, intent: .defaultIntent
              )
        else { return nil }
        return UIImage(cgImage: image, scale: scale, orientation: .up).withRenderingMode(.alwaysTemplate)
    }

    /// A picture's pixels as an image `scale` pixels to the point.
    static func image(_ picture: Picture, scale: CGFloat) -> UIImage? {
        image(rgba: picture.rgba, width: Int(picture.width), height: Int(picture.height), scale: scale)
    }

    static func image(rgba: Data, width: Int, height: Int, scale: CGFloat) -> UIImage? {
        guard width > 0, height > 0, rgba.count == width * height * 4,
              let provider = CGDataProvider(data: rgba as CFData),
              let image = CGImage(
                width: width, height: height, bitsPerComponent: 8, bitsPerPixel: 32, bytesPerRow: width * 4,
                space: CGColorSpaceCreateDeviceRGB(),
                bitmapInfo: CGBitmapInfo(rawValue: CGImageAlphaInfo.premultipliedLast.rawValue),
                provider: provider, decode: nil, shouldInterpolate: true, intent: .defaultIntent
              )
        else { return nil }
        return UIImage(cgImage: image, scale: scale, orientation: .up)
    }
}

/// The marks of threads as the core drew them (`threadMark`): a handful
/// of states at the one or two sizes the app shows them, each kept once.
final class ThreadMarkImages {
    static let shared = ThreadMarkImages()

    struct Mark {
        /// One picture, or one for each step of a mark that moves.
        let frames: [UIImage]
        /// How long each step is held.
        let step: TimeInterval
    }

    private var cache: [String: Mark?] = [:]
    private static let limit = 128

    private init() {
        NotificationCenter.default.addObserver(
            forName: UIApplication.didReceiveMemoryWarningNotification, object: nil, queue: .main
        ) { [weak self] _ in self?.cache.removeAll() }
    }

    func mark(status: String, dot: String, live: Bool, points: CGFloat, scale: CGFloat, dark: Bool) -> Mark? {
        let pixels = UInt32((points * scale).rounded())
        let key = "\(status)|\(dot)|\(live)|\(pixels)|\(dark)"
        if let known = cache[key] { return known }
        if cache.count >= Self.limit { cache.removeAll() }
        var mark: Mark?
        if let drawn = threadMark(status: status, dot: dot, live: live, size: pixels, dark: dark) {
            let side = Int(drawn.side)
            let frames = drawn.frames.compactMap { TabIconImages.image(rgba: $0, width: side, height: side, scale: scale) }
            if !frames.isEmpty, frames.count == drawn.frames.count {
                mark = Mark(frames: frames, step: TimeInterval(drawn.stepMs) / 1000)
            }
        }
        cache[key] = mark
        return mark
    }
}
