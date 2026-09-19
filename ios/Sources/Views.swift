import Foundation

// The App's views, as its JSON names them (thinkterm-web/src/views.rs and
// ui/src/model.ts are the reference). Field names follow the JSON.

struct PaneView: Decodable, Hashable {
    var pane: Int
    var title: String
    var current: Bool
}

struct TabView: Decodable, Hashable, Identifiable {
    var tab: Int
    var window: Int
    var title: String
    var label: String
    var target: Int
    var current: Bool
    var panes: [PaneView]
    var id: Int { tab }
}

struct Controls: Decodable, Hashable {
    var following: Bool
    var fit: Bool
    var closing: Int?
    var clipped: [Int]?
}

struct TabsView: Decodable, Hashable {
    var tabs: [TabView]
    var controls: Controls
}

struct NavRect: Decodable, Hashable {
    var pane: Int
    var left: Double
    var top: Double
    var width: Double
    var height: Double
}

struct CapsuleView: Decodable, Hashable, Identifiable {
    var pane: Int
    var title: String
    var busy: Bool
    var current: Bool
    var id: Int { pane }
}

struct NavView: Decodable, Hashable, Identifiable {
    var rect: NavRect
    var members: [CapsuleView]
    var focused: Bool
    var zoomed: Bool
    var closing: Bool
    var id: Int { rect.pane }
}

struct Toast: Decodable, Hashable {
    var text: String
    var sticky: Bool
    var at: Double
}

struct Card: Decodable, Hashable {
    var title: String
    var hint: String
    var state: String
    var action: String
}

struct StatusView: Decodable, Hashable {
    var toast: Toast?
    var card: Card?
    var summary: String
}

/// One row of the sidebar. Tagged by `kind` in the JSON.
enum SideRow: Decodable, Hashable, Identifiable {
    case space(id: String, name: String)
    case newThread
    case pinned
    case workspaces
    case project(id: String, name: String, path: String, collapsed: Bool, archived: Bool)
    case thread(ThreadRow)
    case archived(count: Int, open: Bool, label: String)
    case others
    case window(id: Int, title: String, selected: Bool)

    var id: String {
        switch self {
        case .space(let id, _): return "space:" + id
        case .newThread: return "new-thread"
        case .pinned: return "pinned"
        case .workspaces: return "workspaces"
        case .project(let id, _, _, _, _): return "project:" + id
        case .thread(let t): return "thread:" + t.id
        case .archived: return "archived"
        case .others: return "others"
        case .window(let id, _, _): return "window:\(id)"
        }
    }

    private enum Keys: String, CodingKey {
        case kind, id, name, path, collapsed, archived, count, open, label, title, selected
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Keys.self)
        switch try c.decode(String.self, forKey: .kind) {
        case "space":
            self = .space(id: try c.decode(String.self, forKey: .id), name: try c.decode(String.self, forKey: .name))
        case "new-thread": self = .newThread
        case "pinned": self = .pinned
        case "workspaces": self = .workspaces
        case "project":
            self = .project(
                id: try c.decode(String.self, forKey: .id),
                name: try c.decode(String.self, forKey: .name),
                path: try c.decode(String.self, forKey: .path),
                collapsed: try c.decode(Bool.self, forKey: .collapsed),
                archived: try c.decode(Bool.self, forKey: .archived)
            )
        case "thread": self = .thread(try ThreadRow(from: decoder))
        case "archived":
            self = .archived(
                count: try c.decode(Int.self, forKey: .count),
                open: try c.decode(Bool.self, forKey: .open),
                label: try c.decode(String.self, forKey: .label)
            )
        case "others": self = .others
        case "window":
            self = .window(
                id: try c.decode(Int.self, forKey: .id),
                title: try c.decode(String.self, forKey: .title),
                selected: try c.decode(Bool.self, forKey: .selected)
            )
        case let other:
            throw DecodingError.dataCorruptedError(forKey: .kind, in: c, debugDescription: "unknown row kind \(other)")
        }
    }
}

struct ThreadRow: Decodable, Hashable {
    var id: String
    var project: String
    var name: String
    var status: String
    var dot: String
    var pinned: Bool
    var unread: Bool
    var live: Bool
    var selected: Bool
    var deleting: Bool
}

struct Editing: Decodable, Hashable {
    var kind: String
    var id: String?
}

struct SidebarView: Decodable, Hashable {
    var rows: [SideRow]
    var editing: Editing
    var space: String?
    var new_project_error: String?
}

struct MenuItem: Decodable, Hashable, Identifiable {
    var id: String
    var label: String
    var icon: String?
    var kind: String
    var enabled: Bool
    var checked: Bool
    var submenu: [MenuItem]

    /// Separators and headers share an empty id; rows need a stable one.
    var rowId: String { id.isEmpty ? kind + ":" + label : id }
}

struct MenuOutcome: Decodable {
    var handled: Bool
    var copy: String?
    var paste: Bool
}

enum ViewJSON {
    static let decoder = JSONDecoder()

    static func decode<T: Decodable>(_ type: T.Type, _ json: String) -> T? {
        guard json != "null", let data = json.data(using: .utf8) else { return nil }
        do {
            return try decoder.decode(type, from: data)
        } catch {
            NSLog("view decode failed: %@ in %@", "\(error)", String(json.prefix(300)))
            return nil
        }
    }
}
