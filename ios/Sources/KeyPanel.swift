import SwiftUI
import UIKit

/// One key, on the bar or in the panel: what it says and what it sends.
struct KeyCap: Identifiable {
    let label: String
    let action: Action
    var id: String { label }

    enum Action {
        /// A key by its DOM name, as the core takes it.
        case dom(name: String, ctrl: Bool, shift: Bool)
        /// Typed text, the character itself.
        case text(String)
        /// Ctrl or Alt held for the next key only.
        case sticky(Sticky)
    }

    enum Sticky {
        case ctrl, alt
    }

    static func dom(_ label: String, _ name: String, ctrl: Bool = false, shift: Bool = false) -> KeyCap {
        KeyCap(label: label, action: .dom(name: name, ctrl: ctrl, shift: shift))
    }

    static func text(_ label: String) -> KeyCap {
        KeyCap(label: label, action: .text(label))
    }
}

/// The key bar's row and the panel's grid, and the one place a cap turns
/// into input.
enum KeyCaps {
    /// The bar under the terminal: the keys a soft keyboard has not got.
    static let bar: [KeyCap] = [
        .dom("esc", "Escape"),
        .dom("tab", "Tab"),
        KeyCap(label: "ctrl", action: .sticky(.ctrl)),
        KeyCap(label: "alt", action: .sticky(.alt)),
        .dom("↑", "ArrowUp"),
        .dom("↓", "ArrowDown"),
        .dom("←", "ArrowLeft"),
        .dom("→", "ArrowRight"),
        .dom("home", "Home"),
        .dom("end", "End"),
        .dom("pgup", "PageUp"),
        .dom("pgdn", "PageDown"),
        .text("-"),
        .text("/"),
        .text("|"),
        .text("~"),
        .dom("^C", "c", ctrl: true),
        .dom("^D", "d", ctrl: true),
        .dom("^L", "l", ctrl: true),
        .dom("^Z", "z", ctrl: true),
        .dom("⌫", "Backspace"),
    ]

    static let functions: [KeyCap] = (1...12).map { .dom("F\($0)", "F\($0)") }

    static let navigation: [KeyCap] = [
        .dom("ins", "Insert"),
        .dom("del", "Delete"),
        .dom("home", "Home"),
        .dom("end", "End"),
        .dom("pgup", "PageUp"),
        .dom("pgdn", "PageDown"),
        .dom("←", "ArrowLeft"),
        .dom("↑", "ArrowUp"),
        .dom("↓", "ArrowDown"),
        .dom("→", "ArrowRight"),
        .dom("⇧tab", "Tab", shift: true),
        .dom("⏎", "Enter"),
    ]

    static let symbols: [KeyCap] = [
        .text("-"), .text("="), .text("/"), .text("|"), .text("~"), .text("^"),
        .text(":"), .text(";"), .text("!"), .text("*"), .text("$"), .text("%"),
        .text("<"), .text(">"), .text("("), .text(")"), .text("{"), .text("}"),
        .text("["), .text("]"), .text("'"), .text("\""), .text("`"), .text("\\"),
    ]

    /// The chords a terminal wants and a phone cannot type.
    static let chords: [KeyCap] = [
        .dom("^A", "a", ctrl: true),
        .dom("^C", "c", ctrl: true),
        .dom("^D", "d", ctrl: true),
        .dom("^E", "e", ctrl: true),
        .dom("^K", "k", ctrl: true),
        .dom("^L", "l", ctrl: true),
        .dom("^R", "r", ctrl: true),
        .dom("^U", "u", ctrl: true),
        .dom("^W", "w", ctrl: true),
        .dom("^X", "x", ctrl: true),
        .dom("^Y", "y", ctrl: true),
        .dom("^Z", "z", ctrl: true),
    ]

    /// Send one cap. Sticky Ctrl and Alt toggle here and are spent by the
    /// model on the next key.
    static func send(_ cap: KeyCap, to model: TerminalModel) {
        switch cap.action {
        case .dom(let name, let ctrl, let shift):
            model.key(name, ctrl: ctrl, shift: shift)
        case .text(let text):
            model.text(text)
            KeyHistory.shared.record(text)
        case .sticky(.ctrl):
            model.ctrlSticky.toggle()
        case .sticky(.alt):
            model.altSticky.toggle()
        }
        tap()
    }

    /// The little knock under a key, when the setting asks for it.
    static func tap() {
        guard AppSettings.shared.hapticKeys else { return }
        UIImpactFeedbackGenerator(style: .light).impactOccurred()
    }
}

/// The four faces of the extension panel.
enum KeyPanelTab: String, CaseIterable, Identifiable {
    case keys, snippets, history, colors

    var id: String { rawValue }

    var title: String {
        switch self {
        case .keys: return tr("p.keys")
        case .snippets: return tr("p.snippets")
        case .history: return tr("p.history")
        case .colors: return tr("p.colors")
        }
    }
}

/// The panel under the key bar: the keys a phone keyboard has not got,
/// the user's snippets, what they sent last, and the colour schemes —
/// the prototype's `.kpanel`, on the terminal's own background.
struct KeyPanel: View {
    @ObservedObject var model: TerminalModel
    @ObservedObject private var lang = AppLanguage.shared
    @Binding var tab: KeyPanelTab
    @ObservedObject private var snippets = SnippetStore.shared
    @ObservedObject private var history = KeyHistory.shared
    @ObservedObject private var settings = AppSettings.shared
    @State private var draft: SnippetDraft?
    /// `--settings` alongside `--keypanel` opens the settings sheet at
    /// launch: the simulator cannot be tapped, and a screenshot needs it.
    @State private var showSettings = ProcessInfo.processInfo.arguments.contains("--settings")

    private let columns = Array(repeating: GridItem(.flexible(), spacing: 8), count: 6)

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 10) {
                chip("gearshape", tr("p.customize")) { showSettings = true }
                chip("doc.on.clipboard", tr("paste")) { model.pasteFromClipboard() }
            }
            .padding(.horizontal, 10)
            .padding(.top, 8)
            .padding(.bottom, 6)

            body(for: tab)
                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)

            Rectangle().fill(Color.white.opacity(0.08)).frame(height: 0.5)

            Picker(tr("p.panel"), selection: $tab) {
                ForEach(KeyPanelTab.allCases) { Text($0.title).tag($0) }
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .padding(.horizontal, 10)
            .padding(.vertical, 8)
        }
        // The panel makes room for the home indicator; the bar above it
        // does not, as the prototype notes.
        .padding(.bottom, 20)
        .frame(height: 268)
        .background(model.background)
        .environment(\.colorScheme, .dark)
        .sheet(item: $draft) { draft in
            SnippetEditor(draft: draft)
        }
        .sheet(isPresented: $showSettings) {
            NavigationStack {
                SettingsView(model: model, showLog: nil)
                    .toolbar {
                        ToolbarItem(placement: .confirmationAction) {
                            Button(tr("done")) { showSettings = false }
                        }
                    }
            }
        }
    }

    @ViewBuilder
    private func body(for tab: KeyPanelTab) -> some View {
        switch tab {
        case .keys: keysGrid
        case .snippets: snippetList
        case .history: historyList
        case .colors: colorRow
        }
    }

    // MARK: keys

    private var keysGrid: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 10) {
                block(tr("k.function"), KeyCaps.functions)
                block(tr("k.navigation"), KeyCaps.navigation)
                block(tr("k.symbols"), KeyCaps.symbols)
                block(tr("k.control"), KeyCaps.chords)
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 6)
        }
    }

    private func block(_ title: String, _ caps: [KeyCap]) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(title.uppercased())
                .font(.system(size: 10, weight: .semibold))
                .kerning(0.6)
                .foregroundColor(.white.opacity(0.45))
            LazyVGrid(columns: columns, spacing: 8) {
                ForEach(caps) { cap in
                    Button { KeyCaps.send(cap, to: model) } label: {
                        Text(cap.label)
                            .font(.system(size: 13, design: .monospaced))
                            .lineLimit(1)
                            .minimumScaleFactor(0.7)
                            .frame(maxWidth: .infinity)
                            .frame(height: 38)
                            .background(Color.white.opacity(0.07))
                            .clipShape(RoundedRectangle(cornerRadius: 10))
                    }
                    .buttonStyle(.plain)
                    .foregroundColor(.white)
                }
            }
        }
    }

    // MARK: snippets

    private var snippetList: some View {
        ScrollView {
            VStack(spacing: 8) {
                ForEach(snippets.items) { snippet in
                    HStack(spacing: 0) {
                        Button { run(snippet) } label: {
                            HStack(spacing: 10) {
                                Text(snippet.text)
                                    .font(.system(size: 13, design: .monospaced))
                                    .lineLimit(1)
                                    .truncationMode(.middle)
                                    .frame(maxWidth: .infinity, alignment: .leading)
                                if snippet.runs {
                                    Text(tr("p.run"))
                                        .font(.system(size: 12))
                                        .foregroundColor(.accentColor)
                                }
                            }
                            .padding(.leading, 12)
                            .padding(.trailing, 8)
                            .frame(height: 42)
                        }
                        .buttonStyle(.plain)
                        .foregroundColor(.white)
                        Button { draft = SnippetDraft(snippet: snippet, isNew: false) } label: {
                            Image(systemName: "slider.horizontal.3")
                                .font(.system(size: 13))
                                .frame(width: 40, height: 42)
                        }
                        .buttonStyle(.plain)
                        .foregroundColor(.white.opacity(0.5))
                    }
                    .background(Color.white.opacity(0.07))
                    .clipShape(RoundedRectangle(cornerRadius: 11))
                }
                Button {
                    draft = SnippetDraft(snippet: Snippet(text: ""), isNew: true)
                } label: {
                    Label(tr("snip.new"), systemImage: "plus")
                        .font(.system(size: 14))
                        .frame(maxWidth: .infinity)
                        .frame(height: 42)
                        .background(Color.white.opacity(0.07))
                        .clipShape(RoundedRectangle(cornerRadius: 11))
                }
                .buttonStyle(.plain)
                .foregroundColor(.white.opacity(0.65))
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 6)
        }
    }

    private func run(_ snippet: Snippet) {
        model.text(snippet.text)
        if snippet.runs { model.key("Enter") }
        KeyHistory.shared.record(snippet.text)
        KeyCaps.tap()
    }

    // MARK: history

    private var historyList: some View {
        ScrollView {
            if history.lines.isEmpty {
                Text(tr("hist.empty"))
                    .font(.system(size: 13))
                    .foregroundColor(.white.opacity(0.45))
                    .frame(maxWidth: .infinity)
                    .padding(.top, 28)
            } else {
                VStack(spacing: 8) {
                    ForEach(history.lines, id: \.self) { line in
                        Button {
                            model.text(line)
                            KeyCaps.tap()
                        } label: {
                            Text(line)
                                .font(.system(size: 13, design: .monospaced))
                                .lineLimit(1)
                                .truncationMode(.middle)
                                .frame(maxWidth: .infinity, alignment: .leading)
                                .padding(.horizontal, 12)
                                .frame(height: 42)
                                .background(Color.white.opacity(0.07))
                                .clipShape(RoundedRectangle(cornerRadius: 11))
                        }
                        .buttonStyle(.plain)
                        .foregroundColor(.white)
                    }
                }
                .padding(.horizontal, 10)
                .padding(.vertical, 6)
            }
        }
    }

    // MARK: colours

    private var colorRow: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            LazyHGrid(rows: [GridItem(.fixed(62)), GridItem(.fixed(62))], spacing: 10) {
                swatch(Schemes.followDesktop, label: tr("followhost"))
                ForEach(Schemes.names, id: \.self) { name in
                    swatch(name, label: name)
                }
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 6)
        }
    }

    /// A scheme's own foreground, so the "Aa" shows on a light swatch too.
    private func foreground(of name: String) -> Color {
        guard let json = Schemes.json(named: name),
              let start = json.range(of: "\"foreground\":\""),
              let end = json[start.upperBound...].firstIndex(of: "\"") else { return .white }
        return Color(hex: String(json[start.upperBound..<end])) ?? .white
    }

    private func swatch(_ name: String, label: String) -> some View {
        let picked = settings.schemeName == name
        return Button {
            settings.schemeName = name
            KeyCaps.tap()
        } label: {
            VStack(spacing: 5) {
                ZStack {
                    RoundedRectangle(cornerRadius: 10)
                        .fill(Schemes.background(of: name) ?? Color.white.opacity(0.12))
                    RoundedRectangle(cornerRadius: 10)
                        .stroke(picked ? Color.accentColor : Color.white.opacity(0.18),
                                lineWidth: picked ? 2 : 1)
                    if name == Schemes.followDesktop {
                        Image(systemName: "desktopcomputer")
                            .font(.system(size: 15))
                            .foregroundColor(.white.opacity(0.8))
                    } else {
                        Text("Aa")
                            .font(.system(size: 13, design: .monospaced))
                            .foregroundColor(foreground(of: name))
                    }
                }
                .frame(width: 64, height: 40)
                Text(label)
                    .font(.system(size: 9.5))
                    .lineLimit(1)
                    .frame(width: 66)
                    .foregroundColor(picked ? .accentColor : .white.opacity(0.6))
            }
        }
        .buttonStyle(.plain)
        .foregroundColor(.white)
    }

    // MARK: chrome

    private func chip(_ icon: String, _ title: String, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            HStack(spacing: 7) {
                Image(systemName: icon).font(.system(size: 13))
                Text(title).font(.system(size: 14))
            }
            .frame(maxWidth: .infinity)
            .frame(height: 36)
            .background(Color.white.opacity(0.07))
            .clipShape(RoundedRectangle(cornerRadius: 10))
        }
        .buttonStyle(.plain)
        .foregroundColor(.white.opacity(0.75))
    }
}

/// The snippet being written, new or old, for the editor's sheet.
struct SnippetDraft: Identifiable {
    var id: UUID { snippet.id }
    var snippet: Snippet
    var isNew: Bool
}

/// One snippet's text and whether it runs itself; Delete throws it away.
struct SnippetEditor: View {
    let draft: SnippetDraft
    @ObservedObject private var lang = AppLanguage.shared
    @Environment(\.dismiss) private var dismiss
    @State private var text: String
    @State private var runs: Bool

    init(draft: SnippetDraft) {
        self.draft = draft
        _text = State(initialValue: draft.snippet.text)
        _runs = State(initialValue: draft.snippet.runs)
    }

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    TextField(tr("snip.command"), text: $text, axis: .vertical)
                        .font(.system(size: 14, design: .monospaced))
                        .lineLimit(1...6)
                        .autocorrectionDisabled()
                        .textInputAutocapitalization(.never)
                    Toggle(tr("snip.enter"), isOn: $runs)
                }
                if !draft.isNew {
                    Section {
                        Button(tr("delete"), role: .destructive) {
                            SnippetStore.shared.remove(draft.snippet)
                            dismiss()
                        }
                    }
                }
            }
            .navigationTitle(draft.isNew ? tr("snip.new") : tr("snip.title"))
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button(tr("cancel")) { dismiss() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button(tr("save")) { save() }
                        .disabled(text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                }
            }
        }
        .presentationDetents([.medium])
    }

    private func save() {
        if draft.isNew {
            SnippetStore.shared.add(text: text, runs: runs)
        } else {
            SnippetStore.shared.replace(Snippet(id: draft.snippet.id, text: text, runs: runs))
        }
        dismiss()
    }
}
