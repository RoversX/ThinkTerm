import SwiftUI

/// The desktop's sidebar and tab strip in one list, the way both are on
/// screen at once on the desktop: the Space, its pinned threads, its
/// projects with their threads, and under an opened thread its tabs, and
/// under a tab with several panes, its panes. Everything is named as the
/// desktop names it: thread, tab, pane.
struct TreeSheet: View {
    @ObservedObject var model: TerminalModel
    @ObservedObject private var lang = AppLanguage.shared
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
            Text(thread.live ? tr(thread.tabs.count == 1 ? "tab.one" : "tab.n", thread.tabs.count) : tr("thread.off"))
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
            AppMenuItems(model: model, kind: "thread", id: thread.id)
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
                    Text(tab.title.isEmpty ? tr("tab.num", tab.tab) : tab.title)
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
                Text(tr("panes.n", tab.panes.count)).font(.caption2).foregroundColor(.secondary)
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
            AppMenuItems(model: model, kind: "tab", id: String(tab.tab))
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

/// The desktop's Live Overview as an overlay under the terminal: a card
/// per thread, grouped by project, two to a row. Each card is the
/// thread's name and state, its tabs as dots (the rest folded into +N),
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
    private let dots = 4
    private let rows = 9
    private let columns = [GridItem(.flexible(), spacing: 10), GridItem(.flexible(), spacing: 10)]

    /// The live card's two frames, reported together once both are known.
    @State private var liveCardFrame: CGRect?
    @State private var thumbFrame: CGRect?

    private func report() {
        guard let card = liveCardFrame, let thumb = thumbFrame else { return }
        let frames = CardFrames(card: card, thumb: thumb)
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
        let shown = Array(thread.tabs.prefix(dots))
        let folded = thread.tabs.count - shown.count
        let currentTab = thread.tabs.first(where: { $0.current }) ?? thread.tabs.first
        let live = thread.id == liveThread
        return Button {
            if !live { model.sideClick("thread", id: thread.id) }
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
                    if live {
                        // The terminal itself lands over this, then fades into
                        // the same preview as every other card; the frame is
                        // reported so the screen above knows where.
                        Color.clear
                            .background(GeometryReader { geo in
                                Color.clear
                                    .onAppear { thumbFrame = geo.frame(in: .named("screen")); report() }
                                    .onChange(of: geo.frame(in: .named("screen"))) { _, f in thumbFrame = f; report() }
                            })
                    }
                }
                .frame(height: 92, alignment: .topLeading)
                .background(thread.live ? model.background : Color.black.opacity(0.4))
                .clipped()
            }
            .background(live ? GeometryReader { geo in
                Color.clear
                    .onAppear { liveCardFrame = geo.frame(in: .named("screen")); report() }
                    .onChange(of: geo.frame(in: .named("screen"))) { _, f in liveCardFrame = f; report() }
            } : nil)
            .background(Color(red: 0.10, green: 0.11, blue: 0.13))
            .clipShape(RoundedRectangle(cornerRadius: 13))
            .overlay(RoundedRectangle(cornerRadius: 13).stroke(live ? Color.accentColor : Color.white.opacity(0.07), lineWidth: live ? 2 : 1))
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
    private let dots = 4

    var body: some View {
        let shown = Array(thread.tabs.prefix(dots))
        let folded = thread.tabs.count - shown.count
        let currentTab = thread.tabs.first(where: { $0.current }) ?? thread.tabs.first
        VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: 7) {
                Circle().fill(TerminalScreen.threadColor(status: thread.status, live: thread.live)).frame(width: 7, height: 7)
                Text(thread.name).font(.system(size: 13.5, weight: .semibold)).lineLimit(1)
                Spacer(minLength: 0)
            }
            .padding(.horizontal, 10)
            .padding(.top, 10)
            .padding(.bottom, 6)
            HStack(spacing: 4) {
                ForEach(shown) { tab in
                    Circle()
                        .fill(thread.current && tab.current ? Color.accentColor : Color.white.opacity(0.28))
                        .frame(width: 6, height: 6)
                }
                if folded > 0 {
                    Text("+\(folded)").font(.system(size: 9.5)).foregroundColor(.secondary)
                }
                Text(currentTab?.title ?? "")
                    .font(.system(size: 10, design: .monospaced))
                    .foregroundColor(.secondary)
                    .lineLimit(1)
                    .padding(.leading, 3)
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
            .padding(.bottom, 7)
        }
        .foregroundColor(.white)
    }
}
