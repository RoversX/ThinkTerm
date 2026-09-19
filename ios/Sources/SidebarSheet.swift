import SwiftUI

/// The desktop's sidebar as a sheet: the Space on show, its projects and
/// their threads, the other windows. Rows come from the App; a tap is
/// the App's own click.
struct SidebarSheet: View {
    @ObservedObject var model: TerminalModel
    @Binding var isPresented: Bool
    @State private var renaming: (kind: String, id: String)?
    @State private var renameText = ""
    @State private var newProjectPath = ""
    @State private var spaceMenu: [MenuItem] = []

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
                    Button { model.sideClick("new-project") } label: { Image(systemName: "folder.badge.plus") }
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
        }
        .onChange(of: model.sidebar?.editing) { _, editing in
            // The App opened a field (a rename, a new project): show it.
            guard let editing else { return }
            switch editing.kind {
            case "thread", "project", "space":
                if renaming == nil {
                    renameText = ""
                    renaming = (editing.kind, editing.id ?? "")
                }
            case "new-project":
                if renaming == nil {
                    renameText = ""
                    renaming = ("new-project", "")
                }
            default:
                break
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
            Text(name).font(.headline).listRowBackground(Color.clear)
        case .newThread:
            Button {
                model.sideClick("new-thread")
                isPresented = false
            } label: {
                Label("New thread", systemImage: "plus")
            }
        case .pinned:
            Text("Pinned").font(.caption).foregroundColor(.secondary)
        case .workspaces:
            Text("Workspaces").font(.caption).foregroundColor(.secondary)
        case .project(let id, let name, let path, let collapsed, _):
            Button {
                model.sideClick("toggle-project", id: id)
            } label: {
                HStack {
                    Image(systemName: collapsed ? "chevron.right" : "chevron.down").font(.caption)
                    VStack(alignment: .leading) {
                        Text(name).font(.subheadline.weight(.semibold))
                        Text(path).font(.caption2).foregroundColor(.secondary).lineLimit(1)
                    }
                    Spacer()
                    Button { model.sideClick("new-thread", id: id) } label: { Image(systemName: "plus") }
                        .buttonStyle(.borderless)
                }
            }
            .contextMenu {
                Button("Rename") { model.sideClick("rename-project", id: id) }
                Button("Archive") { model.sideClick("archive", id: id) }
            }
        case .thread(let t):
            Button {
                model.sideClick("thread", id: t.id)
                isPresented = false
            } label: {
                HStack(spacing: 8) {
                    Circle().fill(dotColor(t)).frame(width: 8, height: 8)
                    Text(t.name).fontWeight(t.unread ? .semibold : .regular)
                    Spacer()
                    if t.pinned { Image(systemName: "pin.fill").font(.caption2).foregroundColor(.secondary) }
                    if !t.live { Text("off").font(.caption2).foregroundColor(.secondary) }
                }
                .padding(.leading, 12)
            }
            .listRowBackground(t.selected ? Color.accentColor.opacity(0.2) : nil)
            .swipeActions(edge: .trailing) {
                Button(role: .destructive) { model.sideClick("delete", id: t.id) } label: { Label("Delete", systemImage: "trash") }
                Button { model.sideClick("pin", id: t.id, flag: !t.pinned) } label: { Label(t.pinned ? "Unpin" : "Pin", systemImage: "pin") }
            }
            .contextMenu {
                Button("Rename") { model.sideClick("rename-thread", id: t.id) }
                Button(t.pinned ? "Unpin" : "Pin") { model.sideClick("pin", id: t.id, flag: !t.pinned) }
                Button("Delete", role: .destructive) { model.sideClick("delete", id: t.id) }
            }
        case .archived(let count, let open, let label):
            Button { model.sideClick("toggle-archived") } label: {
                HStack {
                    Image(systemName: open ? "chevron.down" : "chevron.right").font(.caption)
                    Text("\(label) (\(count))").font(.caption)
                }
            }
        case .others:
            Text("Other windows").font(.caption).foregroundColor(.secondary)
        case .window(let id, let title, let selected):
            Button {
                model.sideClick("window", id: String(id))
                isPresented = false
            } label: {
                HStack {
                    Image(systemName: "macwindow")
                    Text(title.isEmpty ? "Window \(id)" : title)
                }
            }
            .listRowBackground(selected ? Color.accentColor.opacity(0.2) : nil)
        }
    }

    private func dotColor(_ t: ThreadRow) -> Color {
        switch t.status {
        case "Running": return .green
        case "NeedsAttention": return .orange
        case "Done": return .blue
        default: return t.live ? .gray : .gray.opacity(0.4)
        }
    }
}
