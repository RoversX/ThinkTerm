import SwiftUI

/// The desktop's sidebar, as the TUI lays it out: a panel from the left
/// with the tree — the host, its Spaces, their projects, their threads —
/// each level folding, New Thread at the top, Settings at the bottom.
/// Everything is named as the desktop names it.
struct SidebarPanel: View {
    @ObservedObject var model: TerminalModel
    @ObservedObject private var lang = AppLanguage.shared
    @Binding var isPresented: Bool
    var hostName: String
    var connected: Bool
    var onEditHost: () -> Void
    var onSettings: () -> Void
    /// Folded nodes; every Space but the one on show starts folded.
    @State private var folded: Set<String>?
    @State private var renaming: (kind: String, id: String)?
    @State private var renameText = ""

    private var connectionColor: Color {
        if model.reconnecting { return .orange }
        if connected { return .green }
        if model.connection.hasPrefix("connecting") || model.connection.contains("reconnect") { return .orange }
        return .red
    }

    var body: some View {
        ZStack(alignment: .leading) {
            if isPresented {
                Color.black.opacity(0.45)
                    .ignoresSafeArea()
                    .onTapGesture { isPresented = false }
                    .transition(.opacity)
                panel
                    .frame(width: 300)
                    .background(Color(white: 0.09))
                    .ignoresSafeArea(edges: .bottom)
                    .transition(.move(edge: .leading))
            }
        }
        .animation(.easeOut(duration: 0.22), value: isPresented)
        .onChange(of: isPresented) { _, shown in
            if shown {
                model.refreshViews()
                if folded == nil {
                    folded = Set((model.tree?.spaces ?? []).filter { !$0.current }.map { "space:" + $0.id })
                }
            }
        }
        .onChange(of: model.sidebar?.editing) { _, editing in
            // A new project has no id yet: the same field, asking for the
            // name of what is about to exist.
            guard let editing, editing.kind != "none" else { return }
            if renaming == nil {
                renameText = ""
                renaming = (editing.kind, editing.id ?? "")
            }
        }
        .alert(
            renaming?.kind == "new-project" ? tr("project.new") : tr("rename"),
            isPresented: Binding(get: { renaming != nil }, set: { if !$0 { renaming = nil } })
        ) {
            TextField(tr("f.name"), text: $renameText)
            Button(tr("save")) {
                model.sideKey("Enter", value: renameText)
                renaming = nil
            }
            Button(tr("cancel"), role: .cancel) {
                model.sideKey("Escape", value: "")
                renaming = nil
            }
        }
    }

    private func isFolded(_ key: String) -> Bool { folded?.contains(key) ?? false }

    private func toggle(_ key: String) {
        var set = folded ?? []
        if set.contains(key) { set.remove(key) } else { set.insert(key) }
        folded = set
    }

    private var panel: some View {
        VStack(spacing: 0) {
            ScrollView {
                VStack(alignment: .leading, spacing: 0) {
                    // The host: the root of the tree, and the connection's actions.
                    Menu {
                        Button(connected ? tr("disconnect") : tr("reconnect")) {
                            if connected { model.disconnect() } else { model.connect() }
                        }
                        if model.host != nil {
                            Button(tr("edithost")) { onEditHost() }
                        }
                    } label: {
                        treeRow(depth: 0, expandable: true, collapsed: isFolded("host"), onToggle: { toggle("host") }) {
                            Circle().fill(connectionColor).frame(width: 8, height: 8)
                            Text(hostName).fontWeight(.semibold).lineLimit(1)
                            Spacer(minLength: 0)
                            Image(systemName: "ellipsis").foregroundColor(.secondary)
                        }
                    }
                    .buttonStyle(.plain)

                    if !isFolded("host") {
                        Button {
                            model.sideClick("new-thread")
                            isPresented = false
                        } label: {
                            Label(tr("thread.new"), systemImage: "plus")
                                .foregroundColor(.accentColor)
                                .padding(.horizontal, 16)
                                .padding(.vertical, 10)
                        }
                        .buttonStyle(.plain)

                        HStack {
                            Text(tr("workspaces").uppercased()).font(.caption2).foregroundColor(.secondary)
                            Spacer()
                            Button { model.sideClick("new-project") } label: {
                                Image(systemName: "plus").font(.caption).foregroundColor(.secondary).frame(width: 28, height: 28)
                            }
                            .buttonStyle(.plain)
                        }
                        .padding(.leading, 16)
                        .padding(.trailing, 4)

                        ForEach(model.tree?.spaces ?? []) { space in
                            spaceRows(space)
                        }
                        tailRows
                        if let error = model.sidebar?.new_project_error {
                            Text(error).font(.caption).foregroundColor(.red).padding(.horizontal, 16).padding(.vertical, 8)
                        }
                    }
                }
                .padding(.top, 8)
            }
            Divider()
            Button(action: onSettings) {
                Label(tr("settings"), systemImage: "gearshape")
                    .foregroundColor(.secondary)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.horizontal, 16)
                    .padding(.vertical, 12)
            }
            .buttonStyle(.plain)
        }
        .foregroundColor(.white)
    }

    @ViewBuilder
    private func spaceRows(_ space: TreeSpace) -> some View {
        let key = "space:" + space.id
        treeRow(depth: 1, expandable: !space.projects.isEmpty, collapsed: isFolded(key), onToggle: { toggle(key) }) {
            Text(space.name).fontWeight(space.current ? .semibold : .regular).lineLimit(1)
            Spacer(minLength: 0)
            Button {
                model.setSpace(space.id)
                model.sideClick("new-project")
            } label: {
                Image(systemName: "plus").font(.caption).foregroundColor(.secondary).frame(width: 28, height: 28)
            }
            .buttonStyle(.plain)
        }
        .contentShape(Rectangle())
        .onTapGesture {
            if !space.current { model.setSpace(space.id) }
            if isFolded(key) { toggle(key) }
        }
        .contextMenu { AppMenuItems(model: model, kind: "space", id: "") }
        if !isFolded(key) {
            ForEach(space.projects) { project in
                projectRows(space, project)
            }
        }
    }

    @ViewBuilder
    private func projectRows(_ space: TreeSpace, _ project: TreeProject) -> some View {
        let key = "project:" + project.id
        treeRow(depth: 2, expandable: !project.threads.isEmpty, collapsed: isFolded(key), onToggle: { toggle(key) }) {
            Text(project.name).lineLimit(1)
            Spacer(minLength: 0)
            Button {
                model.setSpace(space.id)
                model.sideClick("new-thread", id: project.id)
            } label: {
                Image(systemName: "plus").font(.caption).foregroundColor(.secondary).frame(width: 28, height: 28)
            }
            .buttonStyle(.plain)
        }
        .contentShape(Rectangle())
        .onTapGesture { toggle(key) }
        .contextMenu { AppMenuItems(model: model, kind: "project", id: project.id) }
        if !isFolded(key) {
            ForEach(project.threads) { thread in
                treeRow(depth: 3, expandable: false, collapsed: false, selected: thread.selected, onToggle: {}) {
                    Circle().fill(TerminalScreen.threadColor(status: thread.status, live: thread.live)).frame(width: 8, height: 8)
                    if thread.pinned {
                        Image(systemName: "star.fill").font(.system(size: 9)).foregroundColor(.secondary)
                    }
                    Text(thread.name)
                        .fontWeight(thread.unread || thread.selected ? .semibold : .regular)
                        .foregroundColor(thread.live ? .white : .secondary)
                        .lineLimit(1)
                    Spacer(minLength: 0)
                }
                .contentShape(Rectangle())
                .onTapGesture {
                    model.openThread(thread.id, space: space.id)
                    isPresented = false
                }
                .contextMenu { AppMenuItems(model: model, kind: "thread", id: thread.id) }
            }
        }
    }

    /// What the desktop lists after the tree: archived projects of the
    /// Space on show, and windows no thread claims.
    @ViewBuilder
    private var tailRows: some View {
        let rows = model.sidebar?.rows ?? []
        let tail = Array(rows.drop(while: { row in
            if case .archived = row { return false }
            if case .others = row { return false }
            return true
        }))
        ForEach(tail) { row in
            switch row {
            case .archived(let count, let open, let label):
                Button { model.sideClick("toggle-archived", flag: !open) } label: {
                    HStack(spacing: 6) {
                        Image(systemName: "chevron.right").rotationEffect(.degrees(open ? 90 : 0)).font(.caption)
                        Text("\(label) (\(count))")
                    }
                    .font(.footnote)
                    .foregroundColor(.secondary)
                    .padding(.leading, 16)
                    .padding(.vertical, 6)
                }
                .buttonStyle(.plain)
            case .project(let id, let name, _, _, true):
                treeRow(depth: 2, expandable: false, collapsed: true, onToggle: {}) {
                    Text(name).foregroundColor(.secondary).lineLimit(1)
                    Spacer(minLength: 0)
                }
                .contextMenu { AppMenuItems(model: model, kind: "archived-project", id: id) }
            case .others:
                Text(tr("otherwindows").uppercased()).font(.caption2).foregroundColor(.secondary)
                    .padding(.leading, 16).padding(.top, 10).padding(.bottom, 2)
            case .window(let id, let title, let selected):
                Button {
                    model.sideClick("window", id: String(id))
                    isPresented = false
                } label: {
                    HStack(spacing: 8) {
                        Image(systemName: "macwindow").foregroundColor(.secondary)
                        Text(title.isEmpty ? tr("window.n", id) : title).lineLimit(1)
                        Spacer(minLength: 0)
                    }
                    .padding(.horizontal, 16)
                    .padding(.vertical, 8)
                    .background(selected ? Color.accentColor.opacity(0.18) : Color.clear)
                }
                .buttonStyle(.plain)
            default:
                EmptyView()
            }
        }
    }

    /// One row of the tree: indented by its depth, a fold chevron when it
    /// has children, and its content.
    private func treeRow<Content: View>(
        depth: Int,
        expandable: Bool,
        collapsed: Bool,
        selected: Bool = false,
        onToggle: @escaping () -> Void,
        @ViewBuilder content: () -> Content
    ) -> some View {
        HStack(spacing: 8) {
            if expandable {
                Button(action: onToggle) {
                    Image(systemName: "chevron.down")
                        .font(.caption)
                        .foregroundColor(.secondary)
                        .rotationEffect(.degrees(collapsed ? -90 : 0))
                        .frame(width: 20, height: 20)
                }
                .buttonStyle(.plain)
            } else {
                Spacer().frame(width: 20)
            }
            content()
        }
        .padding(.leading, 8 + CGFloat(depth) * 14)
        .padding(.trailing, 4)
        .padding(.vertical, 6)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(selected ? Color.accentColor.opacity(0.18) : Color.clear)
    }
}
