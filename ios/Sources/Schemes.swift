import SwiftUI

/// The colour schemes in the bundle's schemes.json (the web page's list,
/// straight from the desktop's), by name. Read once, on first use.
enum Schemes {
    static let followDesktop = "desktop"

    /// Every scheme's name, in the file's order (alphabetical).
    static var names: [String] { table.names }

    /// One scheme as the core takes it: its JSON object, unchanged.
    static func json(named name: String) -> String? {
        table.byName[name]
    }

    /// The background of a scheme, for the picker's swatches.
    static func background(of name: String) -> Color? {
        guard let json = table.byName[name],
              let r = json.range(of: "\"background\":\""),
              let end = json[r.upperBound...].firstIndex(of: "\"") else { return nil }
        return Color(hex: String(json[r.upperBound..<end]))
    }

    private struct Table {
        var names: [String] = []
        var byName: [String: String] = [:]
    }

    private static let table: Table = {
        var t = Table()
        guard let url = Bundle.main.url(forResource: "schemes", withExtension: "json"),
              let data = try? Data(contentsOf: url),
              let list = try? JSONSerialization.jsonObject(with: data) as? [[String: Any]] else { return t }
        for entry in list {
            guard let name = entry["name"] as? String,
                  let json = try? JSONSerialization.data(withJSONObject: entry),
                  let text = String(data: json, encoding: .utf8) else { continue }
            t.names.append(name)
            t.byName[name] = text
        }
        return t
    }()
}

extension Color {
    /// "#rrggbb" or "#rgb"; nil for anything else.
    init?(hex: String) {
        var s = hex.trimmingCharacters(in: .whitespaces)
        if s.hasPrefix("#") { s.removeFirst() }
        if s.count == 3 { s = s.map { "\($0)\($0)" }.joined() }
        guard s.count == 6, let v = UInt32(s, radix: 16) else { return nil }
        self.init(
            red: Double((v >> 16) & 0xff) / 255,
            green: Double((v >> 8) & 0xff) / 255,
            blue: Double(v & 0xff) / 255
        )
    }
}
