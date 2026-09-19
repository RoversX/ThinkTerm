import SwiftUI

/// The desktop's sidebar and tab strip in one list, the way both are on
/// screen at once on the desktop: the Space, its pinned threads, its
/// projects with their threads, and under an opened thread its tabs, and
/// under a tab with several panes, its panes. Everything is named as the
/// desktop names it: thread, tab, pane.
struct TreeSheet: View {
    @ObservedObject var model: TerminalModel
    @Binding var isPresented: Bool
    @Binding var menu: MenuSheet?
    @State private var openThreads: Set<String> = []
    @State private var openTabs: Set<String> = []
    @State private var spaceMenu: [MenuItem] = []
    @State private var renaming: (kind: String, id: String)?
    @State private var renameText = ""

    private var threads: [ThreadView] { model.threads?.threads ?? [] }

    var body: some View {
        NavigationStack {
            List {
                ForEach(model.sidebar?.rows ?? []) { row in
                    rowView(row)
                }
                if let error = model.sidebar?.new_project_error {
                    Text(error).font(.caption).foregroundColor(.red)
                }
            }
            .listStyle(.plain)
            .navigationTitle("Threads")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Done") { isPresented = false }
                }
                ToolbarItemGroup(placement: .topBarTrailing) {
                    Menu {
                        ForEach(spaceMenu, id: \.rowId) { item in
                            if item.kind == "item" {
                                Button {
                                    model.menuAction(item.id)
                                    reloadSpaces()
                                } label: {
                                    Label(item.label, systemImage: item.checked ? "checkmark" : "")
                                }
                            }
                        }
                    } label: { Image(systemName: "square.stack.3d.up") }
                    Button { model.chromeClick("new-tab") } label: { Image(systemName: "plus") }
                }
            }
            .alert("Rename", isPresented: Binding(get: { renaming != nil }, set: { if !$0 { renaming = nil } })) {
                TextField("Name", text: $renameText)
                Button("Save") {
                    model.sideKey("Enter", value: renameText)
                    renaming = nil
                }
                Button("Cancel", role: .cancel) {
                    model.sideKey("Escape", value: "")
                    renaming = nil
                }
            }
        }
        .onAppear {
            model.refreshViews()
            reloadSpaces()
            // The current thread starts open: its tabs are what the strip shows.
            if let current = threads.first(where: { $0.current }) {
                openThreads.insert(current.id)
            }
        }
        .onChange(of: model.sidebar?.editing) { _, editing in
            guard let editing, editing.kind != "none", let id = editing.id else { return }
            if renaming == nil {
                renameText = ""
                renaming = (editing.kind, id)
            }
        }
    }

    private func reloadSpaces() {
        spaceMenu = model.contextMenu("space", id: "")
    }

    @ViewBuilder
    private func rowView(_ row: SideRow) -> some View {
        switch row {
        case .space(_, let name):
            Text(name).font(.headline)
        case .newThread:
            Button { model.sideClick("new-thread") } label: {
                Label("New thread", systemImage: "plus").foregroundColor(.accentColor)
            }
        case .pinned:
            caption("Pinned")
        case .workspaces:
            caption("Workspaces")
        case .project(let id, let name, let path, let collapsed, let archived):
            HStack {
                Button { model.sideClick("toggle-project", id: id) } label: {
                    Image(systemName: "chevron.right")
                        .rotationEffect(.degrees(collapsed ? 0 : 90))
                        .foregroundColor(.secondary)
                        .font(.caption)
                }
                .buttonStyle(.plain)
                VStack(alignment: .leading, spacing: 1) {
                    Text(name).font(.subheadline.weight(.semibold))
                    Text(path).font(.caption2).foregroundColor(.secondary).lineLimit(1)
                }
                Spacer()
                if !archived {
                    Button { model.sideClick("new-thread", id: id) } label: { Image(systemName: "plus") }
                        .buttonStyle(.plain)
                        .foregroundColor(.accentColor)
                }
            }
            .contextMenu {
                Button("More…") { menu = MenuSheet(kind: archived ? "archived-project" : "project", id: id, title: name) }
            }
        case .thread(let t):
            if let thread = threads.first(where: { $0.id == t.id }) {
                threadRows(thread, indent: t.project.isEmpty ? 0 : 14)
            } else {
                Text(t.name)
            }
        case .archived(let count, let open, let label):
            Button { model.sideClick("toggle-archived", flag: !open) } label: {
                HStack {
                    Image(systemName: "chevron.right").rotationEffect(.degrees(open ? 90 : 0)).font(.caption)
                    Text("\(label) (\(count))")
                }
                .font(.footnote)
                .foregroundColor(.secondary)
            }
        case .others:
            caption("Other windows")
        case .window(let id, let title, let selected):
            Button { model.sideClick("window", id: String(id)) } label: {
                HStack {
                    Image(systemName: "macwindow").foregroundColor(.secondary)
                    Text(title.isEmpty ? "Window \(id)" : title)
                }
            }
            .listRowBackground(selected ? Color.accentColor.opacity(0.2) : nil)
        }
    }

    private func caption(_ text: String) -> some View {
        Text(text.uppercased()).font(.caption2).foregroundColor(.secondary).padding(.top, 6)
    }

    /// A thread, and when open, its tabs under it.
    @ViewBuilder
    private func threadRows(_ thread: ThreadView, indent: CGFloat) -> some View {
        let open = openThreads.contains(thread.id)
        HStack(spacing: 8) {
            Circle().fill(TerminalScreen.threadColor(status: thread.status, live: thread.live)).frame(width: 8, height: 8)
            Button {
                model.sideClick("thread", id: thread.id)
                isPresented = false
            } label: {
                Text(thread.name)
                    .fontWeight(thread.unread ? .semibold : .regular)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            .buttonStyle(.plain)
            if thread.pinned {
                Image(systemName: "pin").font(.caption).foregroundColor(.secondary)
            }
            Text(thread.live ? "\(thread.tabs.count) tab\(thread.tabs.count == 1 ? "" : "s")" : "off")
                .font(.caption2).foregroundColor(.secondary)
            if !thread.tabs.isEmpty {
                Button {
                    if open { openThreads.remove(thread.id) } else { openThreads.insert(thread.id) }
                } label: {
                    Image(systemName: "chevron.down")
                        .rotationEffect(.degrees(open ? 0 : -90))
                        .font(.caption)
                        .foregroundColor(.secondary)
                        .frame(width: 24, height: 24)
                }
                .buttonStyle(.plain)
            }
        }
        .padding(.leading, indent)
        .listRowBackground(thread.current ? Color.accentColor.opacity(0.18) : nil)
        .contextMenu {
            Button("More…") { menu = MenuSheet(kind: "thread", id: thread.id, title: thread.name) }
        }
        if open {
            ForEach(thread.tabs) { tab in
                tabRows(thread, tab, indent: indent + 20)
            }
        }
    }

    /// A tab, and when open, its panes.
    @ViewBuilder
    private func tabRows(_ thread: ThreadView, _ tab: ThreadTab, indent: CGFloat) -> some View {
        let key = thread.id + ":" + String(tab.tab)
        let open = openTabs.contains(key)
        let current = thread.current && tab.current
        HStack(spacing: 8) {
            Image(systemName: "terminal").foregroundColor(current ? .accentColor : .secondary)
            Button {
                model.chromeClick("pane", pane: tab.target)
                isPresented = false
            } label: {
                HStack(spacing: 6) {
                    Text(tab.title.isEmpty ? "Tab \(tab.tab)" : tab.title)
                    if let first = tab.panes.first, !first.title.isEmpty, first.title != tab.title {
                        Text(first.title).font(.caption).foregroundColor(.secondary)
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
            }
            .buttonStyle(.plain)
            if current {
                Image(systemName: "checkmark").foregroundColor(.accentColor)
            }
            if tab.panes.count > 1 {
                Text("\(tab.panes.count) panes").font(.caption2).foregroundColor(.secondary)
                Button {
                    if open { openTabs.remove(key) } else { openTabs.insert(key) }
                } label: {
                    Image(systemName: "chevron.down")
                        .rotationEffect(.degrees(open ? 0 : -90))
                        .font(.caption)
                        .foregroundColor(.secondary)
                        .frame(width: 24, height: 24)
                }
                .buttonStyle(.plain)
            }
        }
        .padding(.leading, indent)
        .contextMenu {
            Button("Close tab", role: .destructive) { model.chromeClick("close-tab", tab: tab.tab) }
            Button("More…") { menu = MenuSheet(kind: "tab", id: String(tab.tab), title: tab.title) }
        }
        if open {
            ForEach(tab.panes) { pane in
                Button {
                    model.chromeClick("pane", pane: pane.pane)
                    isPresented = false
                } label: {
                    HStack(spacing: 8) {
                        RoundedRectangle(cornerRadius: 1).fill(Color.secondary).frame(width: 6, height: 6)
                        Text(pane.title.isEmpty ? "shell" : pane.title).font(.subheadline)
                        Spacer()
                        if thread.current, model.tabs?.tabs.first(where: { $0.current })?.panes.first(where: { $0.current })?.pane == pane.pane {
                            Image(systemName: "checkmark").foregroundColor(.accentColor)
                        }
                    }
                }
                .buttonStyle(.plain)
                .padding(.leading, indent + 22)
            }
        }
    }
}

/// The desktop's Live Overview, one column of cards: a card per thread,
/// grouped by project, with its tabs as dots (the rest folded into +N),
/// an offline badge, and the last rows of its terminal as a preview.
struct OverviewScreen: View {
    @ObservedObject var model: TerminalModel
    @Binding var isPresented: Bool
    private let dots = 4

    private var threads: [ThreadView] { model.threads?.threads ?? [] }

    private var groups: [(project: String, threads: [ThreadView])] {
        var order: [String] = []
        var byProject: [String: [ThreadView]] = [:]
        for t in threads {
            if byProject[t.project] == nil { order.append(t.project) }
            byProject[t.project, default: []].append(t)
        }
        return order.map { ($0, byProject[$0] ?? []) }
    }

    var body: some View {
        NavigationStack {
            ScrollView {
                if threads.isEmpty {
                    Text("No threads on this server.")
                        .foregroundColor(.secondary)
                        .padding(.top, 60)
                } else {
                    LazyVStack(alignment: .leading, spacing: 10) {
                        ForEach(groups, id: \.project) { group in
                            HStack(spacing: 6) {
                                Text(model.threads?.space ?? "")
                                Text("·").foregroundColor(.secondary)
                                Text(group.project)
                                Rectangle().fill(Color.secondary.opacity(0.3)).frame(height: 1)
                                Text("\(group.threads.count)").foregroundColor(.secondary)
                            }
                            .font(.caption)
                            .padding(.horizontal, 12)
                            .padding(.top, 8)
                            ForEach(group.threads) { thread in
                                card(thread)
                                    .padding(.horizontal, 12)
                            }
                        }
                    }
                    .padding(.vertical, 8)
                }
            }
            .background(Color(white: 0.06))
            .navigationTitle("Overview")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Done") { isPresented = false } }
            }
        }
        .onAppear { model.refreshViews() }
    }

    private func card(_ thread: ThreadView) -> some View {
        let shown = Array(thread.tabs.prefix(dots))
        let folded = thread.tabs.count - shown.count
        let currentTab = thread.tabs.first(where: { $0.current }) ?? thread.tabs.first
        return Button {
            model.sideClick("thread", id: thread.id)
            isPresented = false
        } label: {
            VStack(alignment: .leading, spacing: 6) {
                HStack(spacing: 8) {
                    Circle().fill(TerminalScreen.threadColor(status: thread.status, live: thread.live)).frame(width: 8, height: 8)
                    Text(thread.name).font(.subheadline.weight(.semibold)).lineLimit(1)
                    Spacer()
                }
                HStack(spacing: 4) {
                    ForEach(shown) { tab in
                        Circle()
                            .fill(thread.current && tab.current ? Color.white : Color.white.opacity(0.3))
                            .frame(width: 7, height: 7)
                    }
                    if folded > 0 {
                        Text("+\(folded)").font(.caption2).foregroundColor(.secondary)
                    }
                    Text(currentTab?.title ?? "").font(.caption).foregroundColor(.secondary).lineLimit(1)
                    Spacer()
                    if !thread.live {
                        Text("offline")
                            .font(.caption2)
                            .padding(.horizontal, 6).padding(.vertical, 2)
                            .background(Color.white.opacity(0.1))
                            .clipShape(Capsule())
                    }
                }
                Text(preview(thread))
                    .font(.system(size: 9, design: .monospaced))
                    .foregroundColor(.secondary)
                    .lineLimit(7)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(8)
                    .background(model.background)
                    .clipShape(RoundedRectangle(cornerRadius: 8))
            }
            .padding(10)
            .background(Color.white.opacity(thread.current ? 0.12 : 0.06))
            .clipShape(RoundedRectangle(cornerRadius: 12))
            .overlay(RoundedRectangle(cornerRadius: 12).stroke(thread.current ? Color.accentColor : Color.clear, lineWidth: 1))
        }
        .buttonStyle(.plain)
        .foregroundColor(.white)
    }

    /// The current thread's terminal, its last rows; another thread's is
    /// not on this phone, and shows what it holds.
    private func preview(_ thread: ThreadView) -> String {
        if thread.current, let screen = ViewJSON.decode(ScreenText.self, model.core.screenText()) {
            let rows = screen.text.split(separator: "\n", omittingEmptySubsequences: false)
                .map { $0.replacingOccurrences(of: "\u{2060}", with: "").trimmingCharacters(in: .whitespaces) }
            let kept = rows.reversed().drop(while: { $0.isEmpty }).reversed().suffix(7)
            return kept.joined(separator: "\n")
        }
        return thread.tabs.map { "· " + ($0.title.isEmpty ? "Tab \($0.tab)" : $0.title) }.joined(separator: "\n")
    }
}
