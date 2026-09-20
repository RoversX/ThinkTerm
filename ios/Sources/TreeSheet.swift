import SwiftUI

/// The desktop's sidebar and tab strip in one list, the way both are on
/// screen at once on the desktop: the Space, its pinned threads, its
/// projects with their threads, as far as the desktop's sidebar goes;
/// under a tab with several panes, its panes. Everything is named as the
/// desktop names it: thread, tab, pane.
struct TreeSheet: View {
    @ObservedObject var model: TerminalModel
    @ObservedObject private var lang = AppLanguage.shared
    @Binding var isPresented: Bool
    @Binding var menu: MenuSheet?
    /// The host this sheet belongs to, and what can be done with its
    /// connection (this used to be a dot in the bar).
    var hostName: String = ""
    var connected: Bool = true
    var onEditHost: () -> Void = {}
    @State private var spaceMenu: [MenuItem] = []
    @State private var renaming: (kind: String, id: String)?
    @State private var renameText = ""

    private var threads: [ThreadView] { model.threads?.threads ?? [] }

    private var connectionColor: Color {
        if model.reconnecting { return .orange }
        if connected { return .green }
        if model.connection.hasPrefix("connecting") || model.connection.contains("reconnect") { return .orange }
        return .red
    }

    var body: some View {
        NavigationStack {
            List {
                Section {
                    Menu {
                        Button(connected ? tr("disconnect") : tr("reconnect")) {
                            if connected { model.disconnect() } else { model.connect() }
                        }
                        if model.host != nil {
                            Button(tr("edithost")) { onEditHost() }
                        }
                    } label: {
                        HStack(spacing: 8) {
                            Circle().fill(connectionColor).frame(width: 8, height: 8)
                            Text(hostName).lineLimit(1)
                            Spacer()
                            Text(connected ? tr("disconnect") : tr("reconnect")).font(.subheadline).foregroundColor(.accentColor)
                        }
                    }
                }
                ForEach(model.sidebar?.rows ?? []) { row in
                    rowView(row)
                }
                if let error = model.sidebar?.new_project_error {
                    Text(error).font(.caption).foregroundColor(.red)
                }
            }
            .listStyle(.plain)
            .navigationTitle(tr("threads"))
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button(tr("done")) { isPresented = false }
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
            .alert(tr("rename"), isPresented: Binding(get: { renaming != nil }, set: { if !$0 { renaming = nil } })) {
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
        .onAppear {
            model.refreshViews()
            reloadSpaces()
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
                Label(tr("thread.new"), systemImage: "plus").foregroundColor(.accentColor)
            }
        case .pinned:
            caption(tr("pinned"))
        case .workspaces:
            caption(tr("workspaces"))
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
                AppMenuItems(model: model, kind: archived ? "archived-project" : "project", id: id)
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
            caption(tr("otherwindows"))
        case .window(let id, let title, let selected):
            Button { model.sideClick("window", id: String(id)) } label: {
                HStack {
                    Image(systemName: "macwindow").foregroundColor(.secondary)
                    Text(title.isEmpty ? tr("window.n", id) : title)
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
            if !thread.live {
                Text(tr("thread.off")).font(.caption2).foregroundColor(.secondary)
            }
        }
        .padding(.leading, indent)
        .listRowBackground(thread.current ? Color.accentColor.opacity(0.18) : nil)
        .contextMenu {
            AppMenuItems(model: model, kind: "thread", id: thread.id)
        }
    }

}

/// The desktop's Live Overview as an overlay under the terminal: a card
/// per thread, grouped by project, two to a row. Each card is the
/// thread's name and state with its current tab's title,
/// an offline badge, and a thumbnail of its terminal in its colours: the
/// last rows of its current tab's pane, fetched from the server and
/// refreshed while the overview is up. The thread on show leaves its
/// thumbnail empty and reports the box's frame: the live terminal is
/// zoomed into it by the screen above.
struct OverviewScreen: View {
    @ObservedObject var model: TerminalModel
    @ObservedObject private var lang = AppLanguage.shared
    @Binding var isPresented: Bool
    /// The thread on show's card and its thumbnail box, in the screen's space.
    @Binding var cardFrame: CardFrames?
    var liveThread: String?
    /// The live card's rows wait until the terminal has faded out of it.
    var livePreview: Bool = true
    /// A tapped card, with its frames: the terminal grows back out of it.
    var onPick: (ThreadView, CardFrames?) -> Void = { _, _ in }
    private let rows = 9
    private let columns = [GridItem(.flexible(), spacing: 10), GridItem(.flexible(), spacing: 10)]

    /// Every card's two frames, by thread; the live card's are reported.
    @State private var cardFrames: [String: CGRect] = [:]
    @State private var thumbFrames: [String: CGRect] = [:]

    private func frames(of id: String) -> CardFrames? {
        guard let card = cardFrames[id], let thumb = thumbFrames[id] else { return nil }
        return CardFrames(card: card, thumb: thumb)
    }

    private func report() {
        guard let live = liveThread, let frames = frames(of: live) else { return }
        if cardFrame != frames { cardFrame = frames }
    }

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
        VStack(spacing: 0) {
            ScrollView {
                if threads.isEmpty {
                    Text(tr("overview.none"))
                        .foregroundColor(.secondary)
                        .padding(.top, 60)
                } else {
                    LazyVStack(alignment: .leading, spacing: 8) {
                        ForEach(groups, id: \.project) { group in
                            HStack(spacing: 6) {
                                Text(model.threads?.space ?? "")
                                Text("·").foregroundColor(.secondary)
                                Text(group.project)
                                Rectangle().fill(Color.white.opacity(0.08)).frame(height: 1)
                                Text("\(group.threads.count)").foregroundColor(.secondary)
                            }
                            .font(.caption)
                            .padding(.top, 10)
                            LazyVGrid(columns: columns, spacing: 10) {
                                ForEach(group.threads) { thread in
                                    card(thread)
                                }
                            }
                        }
                    }
                    .padding(.horizontal, 12)
                    .padding(.vertical, 8)
                }
            }
            // Pinching back in returns to the terminal.
            .gesture(MagnifyGesture().onEnded { value in
                if value.magnification > 1.25 { isPresented = false }
            })
        }
        .foregroundColor(.white)
        .background(Color(white: 0.06))
        .onAppear {
            model.refreshViews()
            refreshPreviews()
        }
        .onReceive(Timer.publish(every: 2, on: .main, in: .common).autoconnect()) { _ in
            refreshPreviews()
        }
    }

    /// Every live thread's current tab, asked for its last rows.
    private func refreshPreviews() {
        for thread in threads where thread.live {
            if let pane = previewPane(thread) {
                model.requestPreview(pane: pane, rows: rows)
            }
        }
    }

    private func previewPane(_ thread: ThreadView) -> Int? {
        (thread.tabs.first(where: { $0.current }) ?? thread.tabs.first)?.target
    }

    private func card(_ thread: ThreadView) -> some View {
        let live = thread.id == liveThread
        return Button {
            if !live { model.sideClick("thread", id: thread.id) }
            onPick(thread, frames(of: thread.id))
            isPresented = false
        } label: {
            VStack(alignment: .leading, spacing: 0) {
                ThreadCardHeader(thread: thread)
                ZStack(alignment: .topLeading) {
                    Text(preview(thread))
                        .font(.system(size: 7, design: .monospaced))
                        .lineSpacing(1)
                        .frame(maxWidth: .infinity, alignment: .topLeading)
                        .padding(.horizontal, 8)
                        .padding(.vertical, 7)
                        .opacity(!live || livePreview ? 1 : 0)
                    // The terminal itself lands over this, then fades into the
                    // same preview as every other card; the frame is kept so
                    // the screen above knows where.
                    Color.clear
                        .background(GeometryReader { geo in
                            Color.clear
                                .onAppear { thumbFrames[thread.id] = geo.frame(in: .named("screen")); report() }
                                .onChange(of: geo.frame(in: .named("screen"))) { _, f in thumbFrames[thread.id] = f; report() }
                        })
                }
                .frame(height: 92, alignment: .topLeading)
                .background(thread.live ? model.background : Color.black.opacity(0.4))
                .clipped()
            }
            .background(GeometryReader { geo in
                Color.clear
                    .onAppear { cardFrames[thread.id] = geo.frame(in: .named("screen")); report() }
                    .onChange(of: geo.frame(in: .named("screen"))) { _, f in cardFrames[thread.id] = f; report() }
            })
            .background(Color(red: 0.10, green: 0.11, blue: 0.13))
            .clipShape(RoundedRectangle(cornerRadius: 13))
            // The moving card draws its own border until it has landed.
            .overlay(RoundedRectangle(cornerRadius: 13).stroke(
                live ? (livePreview ? Color.accentColor : Color.clear) : Color.white.opacity(0.07),
                lineWidth: live ? 2 : 1
            ))
        }
        .buttonStyle(.plain)
    }

    /// The rows fetched for the thread's pane in their colours, trailing
    /// blank rows dropped, each cut at about a half-width card's worth.
    private func preview(_ thread: ThreadView) -> AttributedString {
        guard let pane = previewPane(thread), let fetched = model.previews[pane] else { return AttributedString("") }
        let kept = Array(fetched.reversed().drop(while: { $0.runs.isEmpty }).reversed().suffix(rows))
        var out = AttributedString()
        for (i, row) in kept.enumerated() {
            var used = 0
            for run in row.runs {
                let room = 38 - used
                if room <= 0 { break }
                var piece = AttributedString(String(run.text.prefix(room)))
                piece.foregroundColor = Color(hex: run.fg) ?? .white
                out += piece
                used += min(run.text.count, room)
            }
            if i < kept.count - 1 { out += AttributedString("\n") }
        }
        return out
    }
}

/// Where the live card is and where its thumbnail is, in the screen's space.
struct CardFrames: Equatable {
    var card: CGRect
    var thumb: CGRect
}

/// A card's top: the thread's dot and name, its tabs as dots, the current
/// tab's title, and an offline badge. Shared with the screen's zoom, which
/// carries the same header while the terminal shrinks.
struct ThreadCardHeader: View {
    let thread: ThreadView
    @ObservedObject private var lang = AppLanguage.shared

    /// One line: the dot and name, the current tab's title after them, and
    /// an offline badge at the end; the tabs as dots took a row of their own.
    var body: some View {
        let currentTab = thread.tabs.first(where: { $0.current }) ?? thread.tabs.first
        HStack(spacing: 7) {
            Circle().fill(TerminalScreen.threadColor(status: thread.status, live: thread.live)).frame(width: 7, height: 7)
            Text(thread.name).font(.system(size: 13.5, weight: .semibold)).lineLimit(1)
            Text(currentTab?.title ?? "")
                .font(.system(size: 10, design: .monospaced))
                .foregroundColor(.secondary)
                .lineLimit(1)
            Spacer(minLength: 0)
            if !thread.live {
                Text(tr("offline"))
                    .font(.system(size: 9, weight: .semibold))
                    .foregroundColor(.secondary)
                    .padding(.horizontal, 6).padding(.vertical, 2)
                    .background(Color.gray.opacity(0.22))
                    .clipShape(Capsule())
            }
        }
        .padding(.horizontal, 10)
        .padding(.top, 9)
        .padding(.bottom, 8)
        .foregroundColor(.white)
    }
}
