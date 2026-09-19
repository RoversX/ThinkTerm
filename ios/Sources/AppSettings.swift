import SwiftUI
import Combine

/// The app's preferences, one object for every screen: the Settings tab
/// edits them with no terminal open, and an open terminal follows them
/// as they change.
final class AppSettings: ObservableObject {
    static let shared = AppSettings()

    /// A scheme name from schemes.json, or "desktop" to follow the host's.
    @Published var schemeName: String {
        didSet { defaults.set(schemeName, forKey: "scheme.name") }
    }
    /// Smooth (by the pixel, with inertia) or stepped (whole rows).
    @Published var smoothScroll: Bool {
        didSet { defaults.set(smoothScroll, forKey: "scroll.smooth") }
    }
    /// The text size a connection starts at, in points.
    @Published var fontSize: Double {
        didSet { defaults.set(fontSize, forKey: "font.size") }
    }

    private let defaults = UserDefaults.standard

    private init() {
        schemeName = defaults.string(forKey: "scheme.name") ?? Schemes.followDesktop
        smoothScroll = defaults.object(forKey: "scroll.smooth") as? Bool ?? true
        let size = defaults.double(forKey: "font.size")
        fontSize = size > 0 ? size : 11
    }
}
