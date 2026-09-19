import Foundation
import Combine

/// The app's text in the prototype's five languages. The tables ship as
/// Resources/strings.json, built by ios/tools/gen-strings.py; English
/// stands in for a key a table has not got.
enum L10n {
    /// language tag -> key -> text.
    static let tables: [String: [String: String]] = {
        guard let url = Bundle.main.url(forResource: "strings", withExtension: "json"),
              let data = try? Data(contentsOf: url),
              let tables = try? JSONDecoder().decode([String: [String: String]].self, from: data) else {
            NSLog("strings.json is missing: the app shows its keys")
            return [:]
        }
        return tables
    }()

    static let fallback = "en-US"

    /// The phone's own language, matched against the tables once.
    static let systemTag: String = {
        for preferred in Locale.preferredLanguages {
            if let tag = match(preferred) { return tag }
        }
        return fallback
    }()

    /// "zh-Hans-CN" and "zh" both find the "zh-CN" table.
    private static func match(_ language: String) -> String? {
        if tables[language] != nil { return language }
        let code = language.split(separator: "-").first.map(String.init) ?? language
        return tables.keys.sorted().first { $0.split(separator: "-").first.map(String.init) == code }
    }

    /// The tag in force: the phone's own when the setting is "system".
    /// Dates and times are formatted for it too.
    static var currentTag: String {
        let language = AppSettings.shared.language
        return language == "system" ? systemTag : (match(language) ?? fallback)
    }

    static func table(for language: String) -> [String: String] {
        let tag = language == "system" ? systemTag : (match(language) ?? fallback)
        return tables[tag] ?? tables[fallback] ?? [:]
    }

    static func text(_ key: String) -> String {
        if let text = table(for: AppSettings.shared.language)[key] { return text }
        return tables[fallback]?[key] ?? key
    }
}

/// One string by key; any extra arguments fill its %@ and %d.
func tr(_ key: String, _ args: CVarArg...) -> String {
    let format = L10n.text(key)
    return args.isEmpty ? format : String(format: format, arguments: args)
}

/// What the views follow to relabel themselves. AppSettings publishes the
/// chosen tag here, and every view that calls `tr` observes it, so the
/// switch takes effect without a relaunch. Main thread only.
final class AppLanguage: ObservableObject {
    static let shared = AppLanguage()
    @Published var tag: String = AppSettings.shared.language
}
