import SwiftUI

/// The app's preferences, in the prototype's eight sections. The Settings
/// tab shows it with no terminal; a terminal's menu shows it with one,
/// and then the text size is the live pane's and the log is offered.
struct SettingsView: View {
    @ObservedObject var settings = AppSettings.shared
    @ObservedObject private var lang = AppLanguage.shared
    var model: TerminalModel?
    var showLog: Binding<Bool>?

    var body: some View {
        List {
            Section(tr("sec.general")) {
                push(tr("language"), selection: $settings.language, options: Self.languages)
            }

            Section(tr("sec.appearance")) {
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
                if let model {
                    LiveTextSize(model: model)
                } else {
                    Stepper(value: $settings.fontSize, in: 6...40, step: 1) {
                        HStack {
                            Text(tr("textsize"))
                            Spacer()
                            Text(String(format: "%.0f pt", settings.fontSize))
                                .foregroundColor(.secondary)
                                .monospacedDigit()
                        }
                    }
                }
                push(tr("font.family"), selection: $settings.fontFamily, options: Self.fonts)
                push(tr("contrast"), selection: $settings.contrast, options: Self.contrasts)
            }

            Section(tr("sec.terminal")) {
                segment(tr("scroll.mode"), isOn: $settings.smoothScroll, off: tr("scroll.stepped"), on: tr("scroll.smooth"))
                Toggle(tr("scrollbar"), isOn: $settings.scrollbar)
                push(tr("cursor"), selection: $settings.cursorStyle, options: Self.cursors)
                Toggle(tr("cursor.blink"), isOn: $settings.cursorBlink)
                Toggle(tr("bell"), isOn: $settings.bell)
                push(tr("resize.mode"), selection: $settings.resizeMode, options: Self.resizes)
            }

            Section(tr("sec.interface")) {
                segment(tr("i.tabbar"), selection: $settings.tabBarLevels,
                        ("one", tr("i.onelevel")), ("two", tr("i.twolevel")))
                Toggle(tr("i.panebars"), isOn: $settings.paneBars)
            }

            Section(tr("sec.keyboard")) {
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
                Toggle(tr("k.autopanel"), isOn: $settings.autoKeyPanel)
            }

            Section(tr("sec.gestures")) {
                Toggle(tr("g.pinch"), isOn: $settings.pinchZoom)
                Toggle(tr("g.twofinger"), isOn: $settings.twoFingerHidesKeyboard)
                Toggle(tr("g.longpress"), isOn: $settings.longPressSelects)
            }

            Section(tr("sec.connection")) {
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

            if let showLog {
                Section {
                    Toggle(tr("showlog"), isOn: showLog)
                    Toggle(tr("devmode"), isOn: $settings.devMode)
                } header: {
                    Text(tr("diagnostics"))
                } footer: {
                    Text(tr("showlog.foot"))
                }
            }

            Section(tr("sec.about")) {
                HStack {
                    Text(tr("version"))
                    Spacer()
                    Text(Self.version).foregroundColor(.secondary)
                }
            }
        }
        .navigationTitle(tr("settings"))
    }

    // MARK: rows

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
    private func segment(_ title: String, selection: Binding<String>,
                         _ first: (String, String), _ second: (String, String)) -> some View {
        VStack(alignment: .leading, spacing: 7) {
            Text(title).font(.subheadline).foregroundColor(.secondary)
            Picker(title, selection: selection) {
                Text(first.1).tag(first.0)
                Text(second.1).tag(second.0)
            }
            .pickerStyle(.segmented)
            .labelsHidden()
        }
        .padding(.vertical, 3)
    }

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

    static var version: String {
        let info = Bundle.main.infoDictionary
        let short = info?["CFBundleShortVersionString"] as? String ?? "0"
        let build = info?["CFBundleVersion"] as? String ?? "0"
        return "\(short) (\(build))"
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
        .navigationTitle(tr("k.keybar"))
        .navigationBarTitleDisplayMode(.inline)
    }
}

/// The open terminal's text size, stepped live; the size a new
/// connection starts at follows it.
private struct LiveTextSize: View {
    @ObservedObject var model: TerminalModel
    @ObservedObject var settings = AppSettings.shared
    @ObservedObject private var lang = AppLanguage.shared

    var body: some View {
        HStack {
            Text(tr("textsize"))
            Spacer()
            Button { step(-1) } label: { Image(systemName: "textformat.size.smaller") }
                .buttonStyle(.bordered)
            Text(String(format: "%.1f pt", model.fontPt))
                .font(.callout.monospacedDigit())
                .frame(minWidth: 56)
            Button { step(1) } label: { Image(systemName: "textformat.size.larger") }
                .buttonStyle(.bordered)
        }
    }

    private func step(_ by: Double) {
        model.stepFont(by)
        settings.fontSize = max(6, min(40, (model.fontPt * (1 + 0.1 * by)).rounded()))
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
