import Foundation

// The App's views, as its JSON names them (thinkterm-web/src/views.rs and
// ui/src/model.ts are the reference). Field names follow the JSON.

struct PaneView: Decodable, Hashable {
    var pane: Int
    var title: String
    var current: Bool
}

struct TabEntry: Decodable, Hashable, Identifiable {
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
    var tabs: [TabEntry]
    var controls: Controls
    /// The terminal's card, which every window tab wears, as on the desktop.
    var icon: String?
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
    var icon: String?
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

/// The Agents panel: `App::agents_view`.
struct AgentsView: Decodable, Hashable {
    var rows: [AgentRow]
    var summary: String
}

/// One pane an agent runs in.
struct AgentRow: Decodable, Hashable, Identifiable {
    var pane: Int
    /// The agent as its detection names it: "codex", "pi".
    var agent_id: String
    /// Its conversation can be shown as a chat.
    var chat: Bool?
    var name: String
    var title: String
    /// "working", "blocked", "error", "done", "idle" or "unknown".
    var state: String
    var state_label: String
    /// "Project · Thread", or the workspace when no thread claims it.
    var place: String
    var pane_card: String?
    var kind: String?
    var said: String?
    /// How far it got, 0 to 100, when it says.
    var progress: Int?
    /// "7 sub-tasks · 5 working": the one line its sub-tasks fold into;
    /// empty when it has none.
    var subtasks_summary: String
    var subtasks_open: Bool
    /// While that line is open: the sub-tasks listed, and how many more.
    var subtasks: [AgentSubtask]
    var more_subtasks: Int
    var id: Int { pane }
}

/// One of an agent's sub-tasks: "working", "blocked", "error", "done" or
/// "idle", and what it is.
struct AgentSubtask: Decodable, Hashable {
    var state: String
    var label: String
}

/// The panes of the tab on show, each where the split puts it, in cells
/// of a grid `cols` by `rows`: `App::panes_view`.
struct PanesView: Decodable, Hashable {
    var cols: Int
    var rows: Int
    var zoomed: Int?
    var panes: [PaneBox]
}

/// One frame of the split: the pane in front of it, and every pane that
/// shares it.
struct PaneBox: Decodable, Hashable, Identifiable {
    var pane: Int
    var left: Int
    var top: Int
    var cols: Int
    var rows: Int
    var focused: Bool
    var members: [CapsuleView]
    var id: Int { pane }
}

/// The desktop's sidebar layer with its tabs: `App::threads_view`.
struct ThreadsView: Decodable, Hashable {
    var space: String
    var threads: [ThreadView]
}

struct ThreadView: Decodable, Hashable, Identifiable {
    var id: String
    var name: String
    var project: String
    var project_id: String
    var status: String
    var dot: String
    var live: Bool
    var pinned: Bool
    var unread: Bool
    var current: Bool
    var window: Int?
    var tabs: [ThreadTab]
    /// A program in it stopped on an error: it has a mark of its own.
    var failed: Bool?

    /// The status word its mark is drawn for (`threadMark`).
    var markStatus: String { failed == true ? "Failed" : status }
}

struct ThreadTab: Decodable, Hashable, Identifiable {
    var tab: Int
    var title: String
    var target: Int
    var current: Bool
    var panes: [PaneRef]
    var id: Int { tab }
}

struct PaneRef: Decodable, Hashable, Identifiable {
    var pane: Int
    var title: String
    var id: Int { pane }
}

/// The tree whole, every Space with its projects and threads: `App::tree_view`.
struct TreeView: Decodable, Hashable {
    var spaces: [TreeSpace]
}

struct TreeSpace: Decodable, Hashable, Identifiable {
    var id: String
    var name: String
    var current: Bool
    var projects: [TreeProject]
}

struct TreeProject: Decodable, Hashable, Identifiable {
    var id: String
    var name: String
    var path: String
    var threads: [ThreadRow]
}

/// A tab as it is split, for a picture of it: each pane's screen where it
/// lies, in the tab's cells (`App::TabPreview`).
struct TabPreview: Decodable, Hashable {
    var cols: Int
    var rows: Int
    var foreground: String
    var background: String
    var panes: [PanePreview]
}

struct PanePreview: Decodable, Hashable {
    var left: Int
    var top: Int
    var screen: ScreenPreview
}

/// A pane's whole screen: `App::ScreenPreview`.
struct ScreenPreview: Decodable, Hashable {
    var cols: Int
    var rows: Int
    var foreground: String
    var background: String
    var lines: [[ScreenRun]]
}

/// Text from column `col` across `width` columns, of one foreground and
/// background (`bg` is "" on the screen's own).
struct ScreenRun: Decodable, Hashable {
    var text: String
    var fg: String
    var bg: String
    var col: Int
    var width: Int
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
    /// `agent`: the Status panel lists something in it (`ThreadRow.agent`).
    case window(id: Int, title: String, selected: Bool, agent: Bool)

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
        case .window(let id, _, _, _): return "window:\(id)"
        }
    }

    private enum Keys: String, CodingKey {
        case kind, id, name, path, collapsed, archived, count, open, label, title, selected, agent
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
                selected: try c.decode(Bool.self, forKey: .selected),
                agent: try c.decodeIfPresent(Bool.self, forKey: .agent) ?? false
            )
        case let other:
            throw DecodingError.dataCorruptedError(forKey: .kind, in: c, debugDescription: "unknown row kind \(other)")
        }
    }
}

struct ThreadRow: Decodable, Hashable, Identifiable {
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
    /// The cards of what it runs (`ThreadCards`); none from a core before them.
    var icons: [String]?
    /// A program in it stopped on an error: it has a mark of its own.
    var failed: Bool?
    /// The Status panel lists something in it: an agents-only list keeps it.
    var agent: Bool?
    /// The agent in it a chat can be had with, for a tap to open.
    var chat: ThreadChat?

    /// The status word its mark is drawn for (`threadMark`).
    var markStatus: String { failed == true ? "Failed" : status }
}

/// An agent a thread's row opens the chat of: its pane, and the agent as
/// its detection names it ("codex", "pi").
struct ThreadChat: Decodable, Hashable {
    var pane: Int
    var agent: String
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
