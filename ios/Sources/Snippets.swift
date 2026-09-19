import Foundation
import Combine

/// A command the user saved. Tapped in the key panel it goes to the pane
/// as typed text; `runs` adds the Enter that runs it.
struct Snippet: Identifiable, Codable, Equatable {
    var id = UUID()
    var text: String
    var runs: Bool = true
}

/// The saved snippets, as JSON in UserDefaults under "keypanel.snippets".
/// A handful of short strings: kept whole in memory, rewritten on change.
final class SnippetStore: ObservableObject {
    static let shared = SnippetStore()
    private static let key = "keypanel.snippets"

    @Published var items: [Snippet] {
        didSet { save() }
    }

    private init() {
        let data = UserDefaults.standard.data(forKey: Self.key)
        items = data.flatMap { try? JSONDecoder().decode([Snippet].self, from: $0) } ?? Self.starter
    }

    func add(text: String, runs: Bool) {
        let clean = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !clean.isEmpty else { return }
        items.append(Snippet(text: clean, runs: runs))
    }

    func replace(_ snippet: Snippet) {
        guard let i = items.firstIndex(where: { $0.id == snippet.id }) else { return }
        let clean = snippet.text.trimmingCharacters(in: .whitespacesAndNewlines)
        if clean.isEmpty {
            items.remove(at: i)
        } else {
            items[i] = Snippet(id: snippet.id, text: clean, runs: snippet.runs)
        }
    }

    func remove(_ snippet: Snippet) {
        items.removeAll { $0.id == snippet.id }
    }

    private func save() {
        guard let data = try? JSONEncoder().encode(items) else { return }
        UserDefaults.standard.set(data, forKey: Self.key)
    }

    /// What a fresh install starts with, from the prototype's list.
    private static let starter: [Snippet] = [
        Snippet(text: "git status -sb"),
        Snippet(text: "thinkterm cli --prefer-mux list"),
        Snippet(text: "journalctl -u thinkterm-mux -f"),
        Snippet(text: "btop"),
    ]
}

/// The last commands the key panel sent, newest first. The terminal's own
/// model cannot be asked what was typed, so this is only what went through
/// the key bar and the panel; it outlives the app in UserDefaults.
final class KeyHistory: ObservableObject {
    static let shared = KeyHistory()
    private static let key = "keypanel.history"
    private static let limit = 50

    @Published private(set) var lines: [String]

    private init() {
        lines = UserDefaults.standard.stringArray(forKey: Self.key) ?? []
    }

    func record(_ text: String) {
        let line = text.trimmingCharacters(in: .whitespacesAndNewlines)
        // A lone bracket or pipe from the key grid is not worth keeping;
        // a snippet or a pasted command is.
        guard line.count > 1 else { return }
        lines.removeAll { $0 == line }
        lines.insert(line, at: 0)
        if lines.count > Self.limit {
            lines.removeLast(lines.count - Self.limit)
        }
        UserDefaults.standard.set(lines, forKey: Self.key)
    }

    func clear() {
        lines = []
        UserDefaults.standard.removeObject(forKey: Self.key)
    }
}
