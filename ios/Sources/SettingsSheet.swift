import AppLockKit
import SwiftUI

/// The app's preferences, as iOS's own Settings lays them out: a list of
/// what there is to set, each with its symbol, and a page for each. The
/// Settings tab and a terminal's menu show the same; from a terminal it
/// also offers the log. Every open terminal follows a change at once.
struct SettingsView: View {
    @ObservedObject var settings = AppSettings.shared
    @ObservedObject private var lang = AppLanguage.shared
    @Environment(\.dismiss) private var dismiss
    var showLog: Binding<Bool>?

    var body: some View {
        List {
            Section {
                row(.general)
                row(.security)
                row(.appearance)
                row(.terminal)
                row(.interface)
                row(.agents)
            }
            Section {
                row(.keyboard)
                row(.gestures)
                row(.connection)
                NavigationLink {
                    TailnetSettingsView()
                } label: {
                    SettingsRowLabel(symbol: "point.3.connected.trianglepath.dotted", tint: GlassTile.blue, title: "Tailscale")
                }
            }
            Section {
                if showLog != nil { row(.diagnostics) }
                row(.about)
            }
        }
        .onPageBackdrop()
        .navigationTitle(tr("settings"))
    }

    private func row(_ page: SettingsPage) -> some View {
        NavigationLink {
            // Show Welcome leaves Settings altogether, whichever way it came.
            SettingsPageView(page: page, showLog: showLog) { dismiss() }
        } label: {
            SettingsRowLabel(symbol: page.symbol, tint: page.tint, title: page.title)
        }
    }

}

/// What Settings has a page for.
enum SettingsPage {
    case general, security, appearance, terminal, interface, agents, keyboard, gestures, connection, diagnostics, about

    var title: String {
        switch self {
        case .general: return tr("sec.general")
        case .security: return tr("sec.security")
        case .appearance: return tr("sec.appearance")
        case .terminal: return tr("sec.terminal")
        case .interface: return tr("sec.interface")
        case .agents: return tr("sec.agents")
        case .keyboard: return tr("sec.keyboard")
        case .gestures: return tr("sec.gestures")
        case .connection: return tr("sec.connection")
        case .diagnostics: return tr("diagnostics")
        case .about: return tr("sec.about")
        }
    }

    /// The system's symbol for it, as iOS's Settings has one per row.
    var symbol: String {
        switch self {
        case .general: return "gearshape.fill"
        case .security: return "lock.fill"
        case .appearance: return "paintbrush.pointed.fill"
        case .terminal: return "apple.terminal.fill"
        case .interface: return "rectangle.split.2x1.fill"
        case .agents: return "sparkles"
        case .keyboard: return "keyboard.fill"
        case .gestures: return "hand.tap.fill"
        case .connection: return "antenna.radiowaves.left.and.right"
        case .diagnostics: return "stethoscope"
        case .about: return "info.circle.fill"
        }
    }

    /// The colour of its tile, as iOS's Settings colours its rows.
    var tint: Color {
        switch self {
        case .general, .keyboard, .about: return GlassTile.gray
        case .appearance: return GlassTile.blue
        case .terminal: return GlassTile.indigo
        case .interface: return GlassTile.teal
        case .agents: return GlassTile.purple
        case .gestures: return GlassTile.orange
        case .connection, .security: return GlassTile.green
        case .diagnostics: return GlassTile.red
        }
    }
}

/// A row of Settings' list: its symbol on a glass tile of its colour, as
/// iOS's Settings has them, and its name.
struct SettingsRowLabel: View {
    let symbol: String
    let tint: Color
    let title: String

    var body: some View {
        HStack(spacing: 14) {
            GlassTile(symbol: symbol, tint: tint)
            Text(title)
        }
    }
}

/// A symbol in white on a tile of colour, lit as the system's Liquid Glass
/// icons are: the colour a touch lighter at the top and deeper at the
/// foot, a faint sheen over its upper half, and a light rim along the top
/// that fades round the sides.
struct GlassTile: View {
    let symbol: String
    let tint: Color
    var size: CGFloat = 30

    // The system's colours as its Settings icons wear them: the same in the
    // dark, not the brighter shades the dark gives text.
    static let gray = Color(red: 0x8E / 255, green: 0x8E / 255, blue: 0x93 / 255)
    static let blue = Color(red: 0x00 / 255, green: 0x7A / 255, blue: 0xFF / 255)
    static let indigo = Color(red: 0x58 / 255, green: 0x56 / 255, blue: 0xD6 / 255)
    static let teal = Color(red: 0x30 / 255, green: 0xB0 / 255, blue: 0xC7 / 255)
    static let orange = Color(red: 0xFF / 255, green: 0x95 / 255, blue: 0x00 / 255)
    static let green = Color(red: 0x34 / 255, green: 0xC7 / 255, blue: 0x59 / 255)
    static let red = Color(red: 0xFF / 255, green: 0x3B / 255, blue: 0x30 / 255)
    static let purple = Color(red: 0xAF / 255, green: 0x52 / 255, blue: 0xDE / 255)

    var body: some View {
        let shape = RoundedRectangle(cornerRadius: size * 0.27, style: .continuous)
        Image(systemName: symbol)
            .font(.system(size: size * 0.5, weight: .semibold))
            .foregroundStyle(.white)
            .frame(width: size, height: size)
            .background {
                ZStack {
                    shape.fill(LinearGradient(
                        colors: [tint.mix(with: .white, by: 0.1), tint, tint.mix(with: .black, by: 0.08)],
                        startPoint: .top, endPoint: .bottom
                    ))
                    shape.fill(LinearGradient(
                        colors: [.white.opacity(0.12), .white.opacity(0)],
                        startPoint: .top, endPoint: .center
                    ))
                }
            }
            .overlay {
                shape.strokeBorder(
                    LinearGradient(
                        colors: [.white.opacity(0.45), .white.opacity(0.05), .white.opacity(0.15)],
                        startPoint: .top, endPoint: .bottom
                    ),
                    lineWidth: max(0.6, size / 48)
                )
            }
    }
}

/// One of Settings' pages: the preferences it holds, as they were.
struct SettingsPageView: View {
    let page: SettingsPage
    var showLog: Binding<Bool>?
    /// Leaves Settings, for a choice that takes the app elsewhere.
    var close: () -> Void
    @ObservedObject var settings = AppSettings.shared
    @ObservedObject private var lang = AppLanguage.shared
    @ObservedObject private var lock = AppLock.shared

    var body: some View {
        List { content }
            .onPageBackdrop()
            .navigationTitle(page.title)
            .navigationBarTitleDisplayMode(.inline)
    }

    @ViewBuilder
    private var content: some View {
        switch page {
        case .general:
            Section {
                push(tr("language"), selection: $settings.language, options: Self.languages)
            }
        case .security:
            // The app lock (ios/AppLockKit): turned on only once the
            // phone's check passes, so it can always be lifted.
            Section {
                // Either way only once the check passes: off too, or
                // whoever holds the open app could take it off.
                Toggle(tr("lock.app"), isOn: Binding(
                    get: { lock.enabled },
                    set: { on in
                        if on {
                            Task { await lock.turnOn(reason: tr("lock.reason.on")) }
                        } else {
                            Task { await lock.turnOff(reason: tr("lock.reason.off")) }
                        }
                    }
                ))
                if lock.enabled {
                    push(tr("lock.after"), selection: Binding(
                        get: { String(lock.lockAfter) },
                        set: { lock.setLockAfter(Int($0) ?? AppLock.defaultAfter) }
                    ), options: AppLock.choices.map { PickerOption(String($0), tr("lock.after.\($0)")) })
                }
            } footer: {
                // On with no passcode left (turned off since), the app is
                // not held shut: the footer says what puts it back.
                Text(tr(lock.available ? "lock.note.ios" : "lock.nopasscode.ios"))
            }
        case .appearance:
            Section {
                NavigationLink {
                    ThemePicker()
                } label: {
                    HStack {
                        Text(tr("theme"))
                        Spacer()
                        swatch(settings.schemeName)
                        Text(settings.schemeName == Schemes.followDesktop ? tr("followhost") : settings.schemeName)
                            .foregroundColor(.secondary)
                            .lineLimit(1)
                    }
                }
                // The same here and in a terminal's settings: every open
                // terminal takes the size live, and a new connection starts at it.
                Stepper(value: $settings.fontSize, in: 6...40, step: 1) {
                    HStack {
                        Text(tr("textsize"))
                        Spacer()
                        Text(String(format: "%.0f pt", settings.fontSize))
                            .foregroundColor(.secondary)
                            .monospacedDigit()
                    }
                }
                push(tr("font.family"), selection: $settings.fontFamily, options: Self.fonts)
                push(tr("contrast"), selection: $settings.contrast, options: Self.contrasts)
            }
        case .terminal:
            Section {
                segment(tr("scroll.mode"), isOn: $settings.smoothScroll, off: tr("scroll.stepped"), on: tr("scroll.smooth"))
                Toggle(tr("scrollbar"), isOn: $settings.scrollbar)
                push(tr("cursor"), selection: $settings.cursorStyle, options: Self.cursors)
                Toggle(tr("cursor.blink"), isOn: $settings.cursorBlink)
                Toggle(tr("bell"), isOn: $settings.bell)
                push(tr("resize.mode"), selection: $settings.resizeMode, options: Self.resizes)
            }
        case .interface:
            Section {
                Toggle(tr("i.panebars"), isOn: $settings.paneBars)
                push(tr("i.spaces"), selection: $settings.spaces, options: [
                    PickerOption("list", tr("i.spaces.list")),
                    PickerOption("menu", tr("i.spaces.menu")),
                ])
                Toggle(tr("sky.haptics"), isOn: $settings.skyHaptics)
            }
        case .agents:
            Section {
                Toggle(tr("chat.first"), isOn: $settings.chatFirst)
                push(tr("chat.text"), selection: $settings.chatText, options: [
                    PickerOption("s", tr("chat.text.s")),
                    PickerOption("m", tr("chat.text.m")),
                    PickerOption("l", tr("chat.text.l")),
                    PickerOption("xl", tr("chat.text.xl")),
                ])
                push(tr("chat.reveal"), selection: $settings.chatReveal, options: [
                    PickerOption("stream", tr("chat.reveal.stream")),
                    PickerOption("blocks", tr("chat.reveal.blocks")),
                ])
            } footer: {
                Text(tr("chat.first.note"))
            }
        case .keyboard:
            Section {
                NavigationLink {
                    KeyBarKeys()
                } label: {
                    HStack {
                        Text(tr("k.keybar"))
                        Spacer()
                        Text("\(KeyCaps.bar.count)").foregroundColor(.secondary)
                    }
                }
                Toggle(tr("k.haptics"), isOn: $settings.hapticKeys)
            }
        case .gestures:
            Section {
                Toggle(tr("g.pinch"), isOn: $settings.pinchZoom)
                Toggle(tr("g.twofinger"), isOn: $settings.twoFingerHidesKeyboard)
                Toggle(tr("g.longpress"), isOn: $settings.longPressSelects)
            }
        case .connection:
            Section {
                Toggle(tr("c.autoreconnect"), isOn: $settings.autoReconnect)
                Stepper(value: $settings.keepAliveSeconds, in: 0...300, step: 15) {
                    HStack {
                        Text(tr("c.keepalive"))
                        Spacer()
                        Text(settings.keepAliveSeconds == 0 ? tr("off") : "\(settings.keepAliveSeconds) s")
                            .foregroundColor(.secondary)
                            .monospacedDigit()
                    }
                }
                Toggle(tr("c.background"), isOn: $settings.keepSessionInBackground)
            }
        case .diagnostics:
            if let showLog {
                Section {
                    Toggle(tr("showlog"), isOn: showLog)
                    Toggle(tr("devmode"), isOn: $settings.devMode)
                } footer: {
                    Text(tr("showlog.foot"))
                }
            }
        case .about:
            // As the desktop's About page has it: the app and its version,
            // what it is built as and under, where it comes from, and who
            // makes it.
            Section {
                AboutHero()
                    .listRowBackground(Color.clear)
                    .listRowSeparator(.hidden)
            }
            Section {
                aboutRow("shippingbox.fill", GlassTile.blue, tr("version"), About.version)
                // A line of its own only when it says something the version
                // does not.
                if About.build != About.version {
                    aboutRow("arrow.triangle.branch", GlassTile.indigo, tr("about.build"), About.build)
                }
                aboutRow("cpu", GlassTile.teal, tr("about.platform"), About.platform)
                aboutRow("doc.text.fill", GlassTile.orange, tr("about.license"), About.license)
            }
            Section {
                aboutLink(tr("about.source"), About.repo)
                aboutLink(tr("about.releasenotes"), About.repo + "/releases")
                NavigationLink(tr("licenses")) { LicensesView() }
                aboutLink(tr("about.privacy"), About.repo + "/blob/main/PRIVACY.md")
            }
            Section {
                // The first run's sky and pages again, over the app.
                Button(tr("ob.show")) {
                    close()
                    WelcomeAgain.shared.show()
                }
            } footer: {
                // Who makes it, at the foot.
                HStack(alignment: .lastTextBaseline, spacing: 10) {
                    // In the text's own colour, as the desktop draws it; the
                    // year a step quieter.
                    Image("closex-logo")
                        .renderingMode(.template)
                        .resizable()
                        .scaledToFit()
                        .frame(height: 30)
                        .foregroundStyle(Color.primary)
                        .accessibilityLabel("CloseX")
                    Text(verbatim: "© 2026").font(.caption).foregroundStyle(Color.secondary)
                }
                .frame(maxWidth: .infinity)
                .padding(.top, 36)
            }
        }
    }

    // MARK: rows

    /// One of About's facts: its tile, its name, and what it is.
    private func aboutRow(_ symbol: String, _ tint: Color, _ title: String, _ value: String) -> some View {
        HStack(spacing: 14) {
            GlassTile(symbol: symbol, tint: tint)
            Text(title)
            Spacer()
            Text(value)
                .foregroundStyle(.secondary)
                .lineLimit(1)
                .textSelection(.enabled)
        }
    }

    /// A page of the project's, opened in the browser.
    @ViewBuilder
    private func aboutLink(_ title: String, _ address: String) -> some View {
        if let url = URL(string: address) {
            Link(destination: url) {
                HStack {
                    // The text's own colour: a link is a row here, as the
                    // licences' is, not a word in a sentence.
                    Text(title).foregroundStyle(Color.primary)
                    Spacer()
                    Image(systemName: "arrow.up.right")
                        .font(.footnote.weight(.semibold))
                        .foregroundStyle(Color(.tertiaryLabel))
                }
            }
        }
    }

    /// A row that pushes a one-of-these screen and shows the choice.
    private func push(_ title: String, selection: Binding<String>, options: [PickerOption]) -> some View {
        NavigationLink {
            OptionPicker(title: title, options: options, selection: selection)
        } label: {
            HStack {
                Text(title)
                Spacer()
                Text(options.first { $0.value == selection.wrappedValue }?.label ?? selection.wrappedValue)
                    .foregroundColor(.secondary)
                    .lineLimit(1)
            }
        }
    }

    /// The prototype's segmented row: a quiet label with the control under it.
    private func segment(_ title: String, isOn: Binding<Bool>, off: String, on: String) -> some View {
        VStack(alignment: .leading, spacing: 7) {
            Text(title).font(.subheadline).foregroundColor(.secondary)
            Picker(title, selection: isOn) {
                Text(off).tag(false)
                Text(on).tag(true)
            }
            .pickerStyle(.segmented)
            .labelsHidden()
        }
        .padding(.vertical, 3)
    }

    @ViewBuilder
    private func swatch(_ name: String) -> some View {
        if let color = Schemes.background(of: name) {
            Circle().fill(color).overlay(Circle().stroke(Color.white.opacity(0.3), lineWidth: 1)).frame(width: 16, height: 16)
        }
    }

    // MARK: the choices behind the push rows

    private static var languages: [PickerOption] { [
        PickerOption("system", tr("system")),
        PickerOption("en-US", "English"),
        PickerOption("zh-CN", "简体中文"),
        PickerOption("ja-JP", "日本語"),
        PickerOption("fr-FR", "Français"),
        PickerOption("de-DE", "Deutsch"),
    ] }
    // The faces in the bundle; the core shapes from font files, not the
    // system's fonts. Changing one reconnects, since the glyph atlas is
    // built for a face at connect time.
    private static let fonts = ["JetBrains Mono", "Fira Code"].map { PickerOption($0, $0) }
    private static var contrasts: [PickerOption] { [
        PickerOption("off", tr("contrast.off")),
        PickerOption("3", tr("contrast.3")),
        PickerOption("45", tr("contrast.45")),
        PickerOption("7", tr("contrast.7")),
    ] }
    private static var cursors: [PickerOption] { [
        PickerOption("auto", tr("cursor.auto")),
        PickerOption("block", tr("cursor.block")),
        PickerOption("bar", tr("cursor.bar")),
        PickerOption("underline", tr("cursor.underline")),
    ] }
    private static var resizes: [PickerOption] { [
        PickerOption("auto", tr("resize.auto")),
        PickerOption("live", tr("resize.live")),
        PickerOption("release", tr("resize.release")),
    ] }
}

/// What About tells of the app, as the desktop's About page does.
enum About {
    /// The project's home, as the desktop's About links it.
    static let repo = "https://github.com/RoversX/thinkterm"
    /// As the packages declare it, the desktop's About says the same.
    static let license = "GPL-3.0-only"

    static var version: String {
        Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String ?? "0"
    }

    static var build: String {
        Bundle.main.infoDictionary?["CFBundleVersion"] as? String ?? "0"
    }

    /// The system and its version, and the kind of device.
    static var platform: String {
        "\(UIDevice.current.systemName) \(UIDevice.current.systemVersion) · \(UIDevice.current.model)"
    }

    /// The device's own model name ("iPhone17,1"), for a bug report.
    private static var machine: String {
        var info = utsname()
        uname(&info)
        return withUnsafeBytes(of: &info.machine) { raw in
            String(decoding: raw.prefix { $0 != 0 }, as: UTF8.self)
        }
    }

    /// What Copy Info puts on the clipboard: what a bug report needs on its
    /// first lines so nobody has to ask.
    static var info: String {
        [
            "ThinkTerm \(version)",
            "Build: \(build)",
            "OS: \(UIDevice.current.systemName) \(UIDevice.current.systemVersion)",
            "Device: \(machine)",
        ].joined(separator: "\n")
    }
}

/// The head of About: the app's icon, its name, its version, and the way
/// to copy what a bug report needs.
private struct AboutHero: View {
    @ObservedObject private var lang = AppLanguage.shared
    @State private var copied = false

    var body: some View {
        VStack(spacing: 0) {
            Image("app-icon")
                .resizable()
                .scaledToFit()
                .frame(width: 92, height: 92)
                .accessibilityHidden(true)
            Text(verbatim: "ThinkTerm")
                .font(.title.weight(.bold))
                .foregroundStyle(Color.primary)
                .padding(.top, 10)
            Text(tr("about.versionline", About.version))
                .font(.subheadline)
                .foregroundStyle(Color.secondary)
                .padding(.top, 3)
            Button {
                UIPasteboard.general.string = About.info
                copied = true
                DispatchQueue.main.asyncAfter(deadline: .now() + 1.4) { copied = false }
            } label: {
                Label(tr(copied ? "copied" : "about.copy"), systemImage: copied ? "checkmark" : "doc.on.doc")
                    .font(.subheadline.weight(.semibold))
                    .foregroundStyle(Color.primary)
                    .padding(.horizontal, 18)
                    .frame(height: 38)
                    .contentShape(Capsule())
                    .glassCapsule()
            }
            .buttonStyle(.borderless)
            .padding(.top, 14)
        }
        .frame(maxWidth: .infinity)
        .padding(.bottom, 4)
    }
}

/// One choice on a pushed picker screen.
struct PickerOption: Identifiable {
    let value: String
    let label: String
    var id: String { value }

    init(_ value: String, _ label: String) {
        self.value = value
        self.label = label
    }
}

/// The screen a push row opens: the choices, the current one ticked.
struct OptionPicker: View {
    let title: String
    @ObservedObject private var lang = AppLanguage.shared
    let options: [PickerOption]
    @Binding var selection: String

    var body: some View {
        List {
            ForEach(options) { option in
                Button {
                    selection = option.value
                } label: {
                    HStack {
                        Text(option.label).foregroundColor(.primary)
                        Spacer()
                        if selection == option.value {
                            Image(systemName: "checkmark").foregroundColor(.accentColor)
                        }
                    }
                }
            }
        }
        .onPageBackdrop()
        .navigationTitle(title)
        .navigationBarTitleDisplayMode(.inline)
    }
}

/// What the key bar carries, in its order. Read-only for now: the bar is
/// not editable yet.
struct KeyBarKeys: View {
    @ObservedObject private var lang = AppLanguage.shared

    var body: some View {
        List {
            Section {
                ForEach(KeyCaps.bar) { cap in
                    Text(cap.label).font(.system(.body, design: .monospaced))
                }
            } footer: {
                Text(tr("k.keybar.foot"))
            }
        }
        .onPageBackdrop()
        .navigationTitle(tr("k.keybar"))
        .navigationBarTitleDisplayMode(.inline)
    }
}

/// The colour schemes by name, searchable, each with its background as a
/// swatch; the host's own scheme first.
struct ThemePicker: View {
    @ObservedObject var settings = AppSettings.shared
    @ObservedObject private var lang = AppLanguage.shared
    @State private var query = ""

    private var names: [String] {
        let all = Schemes.names
        if query.isEmpty { return all }
        return all.filter { $0.localizedCaseInsensitiveContains(query) }
    }

    var body: some View {
        List {
            if query.isEmpty {
                row(Schemes.followDesktop, label: tr("followhost"), color: nil)
            }
            ForEach(names, id: \.self) { name in
                row(name, label: name, color: Schemes.background(of: name))
            }
        }
        .onPageBackdrop()
        .searchable(text: $query, prompt: tr("theme.search"))
        .navigationTitle(tr("theme"))
        .navigationBarTitleDisplayMode(.inline)
    }

    private func row(_ name: String, label: String, color: Color?) -> some View {
        Button {
            settings.schemeName = name
        } label: {
            HStack {
                if let color {
                    RoundedRectangle(cornerRadius: 4).fill(color)
                        .overlay(RoundedRectangle(cornerRadius: 4).stroke(Color.white.opacity(0.25), lineWidth: 1))
                        .frame(width: 22, height: 22)
                } else {
                    Image(systemName: "desktopcomputer").frame(width: 22)
                }
                Text(label).foregroundColor(.primary)
                Spacer()
                if settings.schemeName == name {
                    Image(systemName: "checkmark").foregroundColor(.accentColor)
                }
            }
        }
    }
}
