import SwiftUI
import Combine

/// The app's preferences, one object for every screen: the Settings tab
/// edits them with no terminal open, and an open terminal follows them
/// as they change. The sections follow the approved prototype; the rows
/// marked "not read yet" are stored so the screen is whole, and the code
/// that honours them comes later.
final class AppSettings: ObservableObject {
    static let shared = AppSettings()

    // MARK: general

    /// A tag of Resources/strings.json, or "system" to follow the phone's.
    /// The views watch AppLanguage, so a change relabels them at once.
    @Published var language: String {
        didSet {
            defaults.set(language, forKey: "lang")
            AppLanguage.shared.tag = language
        }
    }

    // MARK: appearance

    /// A scheme name from schemes.json, or "desktop" to follow the host's.
    @Published var schemeName: String {
        didSet { defaults.set(schemeName, forKey: "scheme.name") }
    }
    /// The text size a connection starts at, in points.
    @Published var fontSize: Double {
        didSet { defaults.set(fontSize, forKey: "font.size") }
    }
    /// One of the bundled faces; a change reconnects to shape with it.
    @Published var fontFamily: String {
        didSet { defaults.set(fontFamily, forKey: "font.family") }
    }
    /// "off", "3", "45" or "7": a WCAG floor text is lifted to.
    @Published var contrast: String {
        didSet { defaults.set(contrast, forKey: "text.contrast") }
    }

    // MARK: terminal

    /// Smooth (by the pixel, with inertia) or stepped (whole rows).
    @Published var smoothScroll: Bool {
        didSet { defaults.set(smoothScroll, forKey: "scroll.smooth") }
    }
    /// A thin bar at the right while the scrollback moves.
    @Published var scrollbar: Bool {
        didSet { defaults.set(scrollbar, forKey: "scroll.bar") }
    }
    /// "auto" (the program's), "block", "bar" or "underline".
    @Published var cursorStyle: String {
        didSet { defaults.set(cursorStyle, forKey: "cursor.style") }
    }
    /// The core blinks the cursor, or holds it steady.
    @Published var cursorBlink: Bool {
        didSet { defaults.set(cursorBlink, forKey: "cursor.blink") }
    }
    /// A pane's bell buzzes the phone.
    @Published var bell: Bool {
        didSet { defaults.set(bell, forKey: "bell") }
    }
    /// "auto" or "live": the server's panes follow a divider drag as it
    /// goes; "release": once, when the finger lifts.
    @Published var resizeMode: String {
        didSet { defaults.set(resizeMode, forKey: "resize.mode") }
    }

    // MARK: interface

    /// "one" or "two" levels of tabs: threads over tabs, or tabs alone.
    @Published var tabBarLevels: String {
        didSet { defaults.set(tabBarLevels, forKey: "ui.tabbar") }
    }
    /// The bars over split panes; off, the rows go to the terminal.
    @Published var paneBars: Bool {
        didSet { defaults.set(paneBars, forKey: "ui.panebars") }
    }

    // MARK: keyboard

    /// A tap on a key taps back. The key bar and the key panel read it.
    @Published var hapticKeys: Bool {
        didSet { defaults.set(hapticKeys, forKey: "key.haptics") }
    }
    /// The key panel opens with the software keyboard.
    @Published var autoKeyPanel: Bool {
        didSet { defaults.set(autoKeyPanel, forKey: "key.autopanel") }
    }

    // MARK: gestures

    /// A pinch on the terminal steps the text size.
    @Published var pinchZoom: Bool {
        didSet { defaults.set(pinchZoom, forKey: "gesture.pinch") }
    }
    /// A two-finger swipe down puts the keyboard away.
    @Published var twoFingerHidesKeyboard: Bool {
        didSet { defaults.set(twoFingerHidesKeyboard, forKey: "gesture.twofinger") }
    }
    /// A long press starts the system's text selection.
    @Published var longPressSelects: Bool {
        didSet { defaults.set(longPressSelects, forKey: "gesture.longpress") }
    }

    // MARK: connection

    /// Redial when the connection drops; off, wait to be asked.
    @Published var autoReconnect: Bool {
        didSet { defaults.set(autoReconnect, forKey: "conn.autoreconnect") }
    }
    /// Seconds between ssh keep-alives, 0 for none; the next connection
    /// takes it.
    @Published var keepAliveSeconds: Int {
        didSet { defaults.set(keepAliveSeconds, forKey: "conn.keepalive") }
    }
    /// Hold the connection when the app goes to the background, for as
    /// long as iOS allows; off, drop it at once and redial on return.
    @Published var keepSessionInBackground: Bool {
        didSet { defaults.set(keepSessionInBackground, forKey: "conn.background") }
    }

    // MARK: diagnostics

    /// The core's counters over the terminal.
    @Published var devMode: Bool {
        didSet { defaults.set(devMode, forKey: "dev.mode") }
    }

    private let defaults = UserDefaults.standard

    private init() {
        let d = UserDefaults.standard
        language = d.string(forKey: "lang") ?? "system"
        schemeName = d.string(forKey: "scheme.name") ?? Schemes.followDesktop
        let size = d.double(forKey: "font.size")
        fontSize = size > 0 ? size : 11
        fontFamily = d.string(forKey: "font.family") ?? "JetBrains Mono"
        contrast = d.string(forKey: "text.contrast") ?? "off"
        smoothScroll = d.object(forKey: "scroll.smooth") as? Bool ?? true
        scrollbar = d.object(forKey: "scroll.bar") as? Bool ?? false
        cursorStyle = d.string(forKey: "cursor.style") ?? "block"
        cursorBlink = d.object(forKey: "cursor.blink") as? Bool ?? true
        bell = d.object(forKey: "bell") as? Bool ?? false
        resizeMode = d.string(forKey: "resize.mode") ?? "auto"
        tabBarLevels = d.string(forKey: "ui.tabbar") ?? "two"
        paneBars = d.object(forKey: "ui.panebars") as? Bool ?? true
        hapticKeys = d.object(forKey: "key.haptics") as? Bool ?? true
        autoKeyPanel = d.object(forKey: "key.autopanel") as? Bool ?? false
        pinchZoom = d.object(forKey: "gesture.pinch") as? Bool ?? true
        twoFingerHidesKeyboard = d.object(forKey: "gesture.twofinger") as? Bool ?? true
        longPressSelects = d.object(forKey: "gesture.longpress") as? Bool ?? true
        autoReconnect = d.object(forKey: "conn.autoreconnect") as? Bool ?? true
        keepAliveSeconds = d.object(forKey: "conn.keepalive") as? Int ?? 30
        keepSessionInBackground = d.object(forKey: "conn.background") as? Bool ?? true
        devMode = d.object(forKey: "dev.mode") as? Bool ?? false
    }
}
