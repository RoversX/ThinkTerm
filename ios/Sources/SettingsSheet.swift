import SwiftUI

/// The app's preferences: the terminal's colours and text size, how a
/// drag scrolls, and the log for when something is wrong. The Settings
/// tab shows it with no terminal; a terminal's menu shows it with one,
/// and then the text size is the live pane's.
struct SettingsView: View {
    @ObservedObject var settings = AppSettings.shared
    var model: TerminalModel?
    var showLog: Binding<Bool>?

    var body: some View {
        List {
            Section("Terminal") {
                NavigationLink {
                    ThemePicker()
                } label: {
                    HStack {
                        Text("Theme")
                        Spacer()
                        swatch(settings.schemeName)
                        Text(settings.schemeName == Schemes.followDesktop ? "Follow host" : settings.schemeName)
                            .foregroundColor(.secondary)
                            .lineLimit(1)
                    }
                }
                if let model {
                    LiveTextSize(model: model)
                } else {
                    Stepper(value: $settings.fontSize, in: 6...40, step: 1) {
                        HStack {
                            Text("Text size")
                            Spacer()
                            Text(String(format: "%.0f pt", settings.fontSize))
                                .foregroundColor(.secondary)
                                .monospacedDigit()
                        }
                    }
                }
                Toggle("Smooth scrolling", isOn: $settings.smoothScroll)
            }
            if let showLog {
                Section {
                    Toggle("Show log", isOn: showLog)
                } header: {
                    Text("Diagnostics")
                } footer: {
                    Text("The connection's log over the terminal, for reporting a problem.")
                }
            }
            Section("About") {
                HStack {
                    Text("Version")
                    Spacer()
                    Text(Self.version).foregroundColor(.secondary)
                }
            }
        }
        .navigationTitle("Settings")
    }

    @ViewBuilder
    private func swatch(_ name: String) -> some View {
        if let color = Schemes.background(of: name) {
            Circle().fill(color).overlay(Circle().stroke(Color.white.opacity(0.3), lineWidth: 1)).frame(width: 16, height: 16)
        }
    }

    static var version: String {
        let info = Bundle.main.infoDictionary
        let short = info?["CFBundleShortVersionString"] as? String ?? "0"
        let build = info?["CFBundleVersion"] as? String ?? "0"
        return "\(short) (\(build))"
    }
}

/// The open terminal's text size, stepped live; the size a new
/// connection starts at follows it.
private struct LiveTextSize: View {
    @ObservedObject var model: TerminalModel
    @ObservedObject var settings = AppSettings.shared

    var body: some View {
        HStack {
            Text("Text size")
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
    @State private var query = ""

    private var names: [String] {
        let all = Schemes.names
        if query.isEmpty { return all }
        return all.filter { $0.localizedCaseInsensitiveContains(query) }
    }

    var body: some View {
        List {
            if query.isEmpty {
                row(Schemes.followDesktop, label: "Follow host", color: nil)
            }
            ForEach(names, id: \.self) { name in
                row(name, label: name, color: Schemes.background(of: name))
            }
        }
        .searchable(text: $query, prompt: "Search schemes")
        .navigationTitle("Theme")
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
