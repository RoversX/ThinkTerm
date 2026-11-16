import SwiftUI
import UIKit

/// Settings, About, Open Source Licenses: what the app ships that is not
/// its own, each with its license as its authors wrote it. The list is
/// licenses.json, which ios/build.sh makes (ios/tools/gen-licenses.py)
/// from the crates linked into the core, the bundle's fonts and icons, and
/// the repository's NOTICE.
struct LicensesView: View {
    @ObservedObject private var lang = AppLanguage.shared
    @State private var query = ""

    private var items: [LicenseList.Item] {
        let all = LicenseList.shared.items
        if query.isEmpty { return all }
        return all.filter { item in
            item.name.localizedCaseInsensitiveContains(query)
                || item.license.localizedCaseInsensitiveContains(query)
                || (item.includes ?? []).contains { $0.localizedCaseInsensitiveContains(query) }
        }
    }

    var body: some View {
        let shown = items
        List {
            section("app", title: "ThinkTerm", in: shown)
            section("assets", title: tr("licenses.assets"), in: shown)
            section("rust", title: tr("licenses.rust"), in: shown)
        }
        .searchable(text: $query, prompt: tr("licenses.search"))
        .navigationTitle(tr("licenses"))
        .navigationBarTitleDisplayMode(.inline)
    }

    @ViewBuilder
    private func section(_ key: String, title: String, in shown: [LicenseList.Item]) -> some View {
        let rows = shown.filter { $0.section == key }
        if !rows.isEmpty {
            Section(title) {
                ForEach(rows) { item in
                    NavigationLink {
                        LicenseDetail(item: item)
                    } label: {
                        VStack(alignment: .leading, spacing: 2) {
                            Text(item.title)
                            Text(item.license).font(.caption).foregroundColor(.secondary)
                        }
                    }
                }
            }
        }
    }
}

/// One component's licenses, as one scrolling text: who it is, where its
/// source is, then each license file it ships.
private struct LicenseDetail: View {
    let item: LicenseList.Item

    var body: some View {
        PlainText(text: text)
            .ignoresSafeArea(edges: .bottom)
            .navigationTitle(item.name)
            .navigationBarTitleDisplayMode(.inline)
    }

    private var text: NSAttributedString {
        let body = UIFont.preferredFont(forTextStyle: .footnote)
        let head = UIFont.preferredFont(forTextStyle: .headline)
        let out = NSMutableAttributedString()
        func add(_ string: String, _ font: UIFont, _ color: UIColor = .label) {
            out.append(NSAttributedString(string: string, attributes: [.font: font, .foregroundColor: color]))
        }
        add(item.title + "\n", head)
        add(item.license + "\n", body, .secondaryLabel)
        if let url = item.url, !url.isEmpty {
            add(url + "\n", body, .secondaryLabel)
        }
        if let includes = item.includes, !includes.isEmpty {
            add("\n" + tr("licenses.includes", includes.joined(separator: ", ")) + "\n", body, .secondaryLabel)
        }
        for id in item.texts {
            guard let license = LicenseList.shared.texts[id] else { continue }
            add("\n\n" + license + "\n", body)
        }
        return out
    }
}

/// A long, read-only, selectable text that scrolls; links open. UIKit's,
/// as one license file alone (the Rust standard library's) is too long for
/// a SwiftUI Text to lay out quickly.
private struct PlainText: UIViewRepresentable {
    let text: NSAttributedString

    func makeUIView(context: Context) -> UITextView {
        let view = UITextView()
        view.isEditable = false
        view.isSelectable = true
        view.dataDetectorTypes = .link
        view.backgroundColor = .systemBackground
        view.textContainerInset = UIEdgeInsets(top: 16, left: 12, bottom: 24, right: 12)
        view.adjustsFontForContentSizeCategory = true
        return view
    }

    func updateUIView(_ view: UITextView, context: Context) {
        if view.attributedText != text {
            view.attributedText = text
        }
    }
}

/// licenses.json, read once.
struct LicenseList: Decodable {
    struct Item: Decodable, Identifiable {
        /// "app" (ThinkTerm and the code it is built from), "assets" (the
        /// fonts and icons) or "rust" (the crates).
        let section: String
        let name: String
        let version: String?
        let license: String
        let url: String?
        /// The crates one entry stands for (WezTerm's, a vendored project's).
        let includes: [String]?
        /// Keys into `texts`.
        let texts: [String]

        var id: String { "\(section)/\(name)/\(version ?? "")" }
        var title: String { version.map { "\(name) \($0)" } ?? name }
    }

    let items: [Item]
    let texts: [String: String]

    static let shared: LicenseList = {
        guard let url = Bundle.main.url(forResource: "licenses", withExtension: "json"),
              let data = try? Data(contentsOf: url),
              let list = try? JSONDecoder().decode(LicenseList.self, from: data)
        else {
            return LicenseList(items: [], texts: [:])
        }
        return list
    }()
}
