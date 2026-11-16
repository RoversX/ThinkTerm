import SwiftUI

/// Where the app opens: every host's threads on one screen, each host as
/// the desktop's sidebar lists it -- its projects, their threads with the
/// desktop's marks -- under glass bars for finding a thread and starting
/// one. A thread tapped opens its host's terminal on it; a host this phone
/// has been in before is connected to as the screen comes up, and one it
/// has not waits for a tap.
struct ThreadsHome: View {
    @EnvironmentObject var store: HostStore
    @EnvironmentObject var sessions: Sessions
    @ObservedObject private var lang = AppLanguage.shared
    @State private var path = NavigationPath()
    @State private var searching = false
    @State private var query = ""
    @FocusState private var searchFocused: Bool
    @State private var adding = false
    /// The hosts connected now, as their rows say: a thread can be
    /// started on one of them.
    @State private var connectedHosts: Set<UUID> = []
    /// Where the system puts bars along the top and bottom (`barRoom`).
    @State private var headerRoom = EdgeInsets()
    @State private var footerRoom = EdgeInsets()

    private static let backdrop = Color.chromeBackdrop
    private static let headerHeight: CGFloat = 64
    private static let footerHeight: CGFloat = 64

    /// What the list leads to.
    private enum Route: Hashable {
        /// A host's terminal, by the host's id (the probe's is `Sessions.probeId`).
        case terminal(UUID)
        case hosts
        case settings
    }

    /// A host's place in the list: a saved host, or in debug builds the
    /// probe on this Mac, which has no row among the saved ones.
    private struct Entry: Identifiable {
        let id: UUID
        let name: String
        let host: Host?
    }

    private var entries: [Entry] {
        var out = store.hosts.map { Entry(id: $0.id, name: $0.display, host: $0) }
        #if DEBUG
        if Self.probeIsUp {
            out.append(Entry(id: Sessions.probeId, name: tr("thismac"), host: nil))
        }
        #endif
        return out
    }

    #if DEBUG
    /// The probe's sshd left its key: `ios/tools/probe.sh up` has run.
    private static let probeIsUp = FileManager.default.fileExists(atPath: "/tmp/ttp-ssh/userkey")
    #endif

    var body: some View {
        NavigationStack(path: $path) {
            list
                .toolbar(.hidden, for: .navigationBar)
                .navigationDestination(for: Route.self) { route in
                    switch route {
                    case .terminal(let id):
                        TerminalScreen(model: model(id))
                    case .hosts:
                        HostsView()
                    case .settings:
                        SettingsView(showLog: nil)
                    }
                }
        }
        .sheet(isPresented: $adding) {
            HostEditView(host: Host()) { host, secret, passphrase in
                Keychain.save(secret, account: host.id.uuidString)
                if let passphrase { Keychain.save(passphrase, account: host.id.uuidString + ".passphrase") }
                store.upsert(host)
            }
            .environmentObject(store)
        }
        .onAppear {
            // A saved host for the probe, with its key in the Keychain,
            // opened over the list: the same path a real host takes, back
            // included.
            if path.isEmpty, let seeded = ThinkTermApp.seededHost(into: store) {
                path.append(Route.terminal(seeded.id))
            }
            connectKnownHosts()
        }
    }

    /// The session of the host with `id`, made when there is none.
    private func model(_ id: UUID) -> TerminalModel {
        sessions.model(for: store.hosts.first { $0.id == id }, store: store)
    }

    /// A host this phone has connected to before has its key approved and
    /// its secret kept: it is connected to without being asked, so its
    /// threads are in the list. One never connected to is left for a tap.
    private func connectKnownHosts() {
        for entry in entries where sessions.existing(entry.id) == nil {
            guard entry.host == nil || entry.host?.lastConnected != nil else { continue }
            model(entry.id).connect()
        }
    }

    private var list: some View {
        ScrollView {
            LazyVStack(alignment: .leading, spacing: 2) {
                if entries.isEmpty {
                    HostsEmptyState { adding = true }
                        .padding(.top, 40)
                } else {
                    ForEach(entries) { entry in
                        HostThreads(
                            name: entry.name,
                            model: sessions.existing(entry.id),
                            query: searching ? query : "",
                            connect: { model(entry.id).connect() },
                            open: { path.append(Route.terminal(entry.id)) },
                            connected: { up in
                                if up { connectedHosts.insert(entry.id) } else { connectedHosts.remove(entry.id) }
                            }
                        )
                    }
                }
            }
            .padding(.horizontal, 10)
            .padding(.bottom, 12)
        }
        .scrollDismissesKeyboard(.immediately)
        .foregroundColor(.primary)
        .background(Self.backdrop.ignoresSafeArea())
        .floatingBar(edge: .top, fading: Self.backdrop, scrim: true) {
            VStack(spacing: 0) {
                header
                if searching { searchField }
            }
        }
        .floatingBar(edge: .bottom, fading: Self.backdrop) {
            if !searching { footer }
        }
        .barRoom(.top, height: Self.headerHeight, $headerRoom)
        .barRoom(.bottom, height: Self.footerHeight, $footerRoom)
    }

    // MARK: the bars

    private var header: some View {
        HStack(spacing: 10) {
            Text(tr("threads"))
                .font(.largeTitle.weight(.bold))
                .lineLimit(1)
            Spacer(minLength: 0)
            Button {
                withAnimation(.easeOut(duration: 0.2)) {
                    searching.toggle()
                    if !searching { query = "" }
                }
                searchFocused = searching
            } label: {
                DesktopIcon(name: searching ? "x" : "search")
                    .frame(width: 44, height: 44)
                    .contentShape(Circle())
            }
            .buttonStyle(.plain)
            .glassCircle()
            .accessibilityLabel(tr(searching ? "cancel" : "thread.find"))
        }
        .foregroundColor(.primary)
        .padding(.leading, max(20, headerRoom.leading))
        .padding(.trailing, 14)
        .padding(.top, 6 + max(0, headerRoom.top))
        .padding(.bottom, 10)
    }

    private var searchField: some View {
        HStack(spacing: 8) {
            DesktopIcon(name: "search", size: 18).foregroundColor(.secondary)
            TextField(tr("thread.find"), text: $query)
                .focused($searchFocused)
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
                .submitLabel(.search)
        }
        .foregroundColor(.primary)
        .padding(.horizontal, 14)
        .frame(height: 40)
        .glassCapsule()
        .padding(.horizontal, 14)
        .padding(.bottom, 8)
        .transition(.move(edge: .top).combined(with: .opacity))
    }

    /// A new thread, where the thumb is, then the hosts and the settings.
    private var footer: some View {
        HStack(spacing: 12) {
            newThread
            Spacer(minLength: 0)
            HStack(spacing: 10) {
                Button { path.append(Route.hosts) } label: {
                    DesktopIcon(name: "server", size: 22)
                        .frame(width: 50, height: 50)
                        .contentShape(Circle())
                }
                .buttonStyle(.plain)
                .glassCircle()
                .accessibilityLabel(tr("hosts"))
                Button { path.append(Route.settings) } label: {
                    DesktopIcon(name: "settings", size: 22)
                        .frame(width: 50, height: 50)
                        .contentShape(Circle())
                }
                .buttonStyle(.plain)
                .glassCircle()
                .accessibilityLabel(tr("settings"))
            }
            .glassGroup(spacing: 10)
        }
        .foregroundColor(.primary)
        .dynamicTypeSize(...DynamicTypeSize.xxLarge)
        .padding(.leading, max(14, footerRoom.leading))
        .padding(.trailing, 14)
        .padding(.top, 6)
        .padding(.bottom, 8 + footerRoom.bottom)
    }

    /// The hosts a thread can be started on: the connected ones.
    private var connected: [Entry] {
        entries.filter { connectedHosts.contains($0.id) }
    }

    /// With one host connected the button starts a thread there; with
    /// more it asks which; with none there is nowhere to start one.
    @ViewBuilder
    private var newThread: some View {
        let hosts = connected
        let label = HStack(spacing: 8) {
            DesktopIcon(name: "square-pen")
            Text(tr("thread.new"))
                .font(.body.weight(.semibold))
                .lineLimit(1)
                .minimumScaleFactor(0.8)
        }
        // On the accent's blue, in either appearance.
        .foregroundColor(.white)
        .padding(.horizontal, 18)
        .frame(height: 50)
        .contentShape(Capsule())
        if hosts.count == 1, let only = hosts.first {
            Button { startThread(on: only) } label: { label }
                .buttonStyle(.plain)
                .glassCapsule(tint: .accentColor)
        } else if hosts.count > 1 {
            Menu {
                ForEach(hosts) { entry in
                    Button(entry.name) { startThread(on: entry) }
                }
            } label: { label }
            .buttonStyle(.plain)
            .glassCapsule(tint: .accentColor)
        }
    }

    private func startThread(on entry: Entry) {
        model(entry.id).sideClick("new-thread")
        path.append(Route.terminal(entry.id))
    }
}

/// One host in the list: its name and how its connection stands, then its
/// projects and their threads as the sidebar draws them. A host with no
/// session, or one that is not connected, offers to connect in their
/// place; while a search is on, only the threads it finds are listed.
private struct HostThreads: View {
    var name: String
    var model: TerminalModel?
    var query: String
    var connect: () -> Void
    var open: () -> Void
    var connected: (Bool) -> Void

    var body: some View {
        if let model {
            Connected(name: name, model: model, query: query, connect: connect, open: open, connected: connected)
        } else if query.isEmpty {
            HostTitle(name: name, color: Color.primary.opacity(0.25))
            SidebarRow(icon: "link-2", title: tr("connect"), dim: true, action: connect)
        }
    }
}

/// The host's name, the tree's root, with the dot of its connection.
private struct HostTitle: View {
    var name: String
    var color: Color
    var busy = false

    var body: some View {
        HStack(spacing: 8) {
            Text(name)
                .font(.title3.weight(.bold))
                .lineLimit(1)
            if busy {
                Spinner(color: .orange).frame(width: 12, height: 12)
            } else {
                Circle().fill(color).frame(width: 8, height: 8)
            }
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 12)
        .padding(.top, 18)
        .padding(.bottom, 4)
        .accessibilityElement(children: .combine)
        .accessibilityAddTraits(.isHeader)
    }
}

/// A host with a session: what its tree holds, kept current while the
/// list is up -- the session's own frames, which read the views, rest
/// while its terminal is off the screen.
/// What the App asks of the user from a row's menu here -- a new name, or
/// whether to delete a thread -- asked here, not later on the terminal's
/// screen; that screen asks for itself while it shows the host (the list
/// stays under it, out of sight).
private struct HomeAsks: ViewModifier {
    @ObservedObject var model: TerminalModel
    @State private var renaming: String?
    @State private var renameText = ""

    private var deleting: Bool { model.confirming != nil && !model.isOnScreen }

    private func threadName(_ id: String) -> String {
        for space in model.tree?.spaces ?? [] {
            for project in space.projects {
                if let thread = project.threads.first(where: { $0.id == id }) { return thread.name }
            }
        }
        return ""
    }

    func body(content: Content) -> some View {
        content
            .onChange(of: model.sidebar?.editing) { _, editing in
                guard let editing, editing.kind != "none", !model.isOnScreen, renaming == nil else { return }
                renameText = ""
                renaming = editing.kind
            }
            .alert(
                renaming == "new-project" ? tr("project.new") : tr("rename"),
                isPresented: Binding(get: { renaming != nil }, set: { if !$0 { renaming = nil } })
            ) {
                TextField(renaming == "new-project" ? "~/" : tr("f.name"), text: $renameText)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                Button(tr("save")) {
                    model.sideKey("Enter", value: renameText)
                    renaming = nil
                }
                Button(tr("cancel"), role: .cancel) {
                    model.sideKey("Escape", value: "")
                    renaming = nil
                }
            }
            .confirmationDialog(
                tr("thread.delete.title", threadName(model.confirming?.split(separator: ":", maxSplits: 1).last.map(String.init) ?? "")),
                isPresented: Binding(get: { deleting }, set: { if !$0 { model.confirming = nil } }),
                titleVisibility: .visible
            ) {
                // Deleting ends every program in the thread: it is asked first.
                Button(tr("delete"), role: .destructive) {
                    if let action = model.confirming { model.menuAction(action) }
                    model.confirming = nil
                }
                Button(tr("cancel"), role: .cancel) { model.confirming = nil }
            } message: {
                Text(tr("thread.delete.body"))
            }
    }
}

private struct Connected: View {
    var name: String
    @ObservedObject var model: TerminalModel
    @ObservedObject private var settings = AppSettings.shared
    var query: String
    var connect: () -> Void
    var open: () -> Void
    var connected: (Bool) -> Void
    /// Folded projects.
    @State private var folded: Set<String> = []

    private var spaces: [TreeSpace] { model.tree?.spaces ?? [] }

    /// The App keeps the pane's status while its connection is lost. With
    /// the setting off nothing redials it: not ready, not on its way, and
    /// the way back is a tap.
    private var lost: Bool { model.reconnecting && !settings.autoReconnect }

    /// Attached to the server: its threads are there to list. The core
    /// says which pane it shows once it is.
    private var ready: Bool { model.connection.hasPrefix("pane ") && !lost }

    /// On the way there: dialling, or connected and attaching.
    private var connecting: Bool {
        !lost && ((model.reconnecting && settings.autoReconnect) || (model.isConnected && !ready) || model.connection.contains("reconnect"))
    }

    private var dot: Color {
        if ready { return .green }
        // Waiting on the user, not broken.
        if model.pendingHostKey != nil { return .orange }
        return model.connection.hasPrefix("failed") ? .red : Color.primary.opacity(0.25)
    }

    var body: some View {
        // One view, not a group of rows: a task or a change watcher on a
        // group runs once for each row in it, and every row's going ran
        // the listing's end while the others still showed. The list's
        // spacing, kept inside it.
        VStack(alignment: .leading, spacing: 2) {
            if !query.isEmpty {
                found
            } else {
                HostTitle(name: name, color: dot, busy: connecting && model.pendingHostKey == nil)
                    .contentShape(Rectangle())
                    .onTapGesture { if ready { open() } }
                    .contextMenu {
                        let up = model.isConnected && !lost
                        Button(up ? tr("disconnect") : tr("reconnect")) {
                            if up { model.disconnect() } else { connect() }
                        }
                    }
                if model.pendingHostKey != nil {
                    // The key is shown, and trusted or not, on its screen.
                    SidebarRow(icon: "key-round", title: tr("hostkey.approve.title"), bold: true, action: open)
                } else if !ready {
                    if connecting {
                        SidebarRow(icon: "link-2", title: tr("connecting"), dim: true) {}
                    } else {
                        SidebarRow(icon: "rotate-ccw", title: tr("reconnect"), dim: true, action: connect)
                    }
                } else {
                    tree
                }
            }
        }
        .onChange(of: ready, initial: true) { _, up in connected(up) }
        .modifier(HomeAsks(model: model))
        .task {
            // The views are kept while the list shows them, and read as the
            // core says they changed (`TerminalModel.setViewsListed`).
            model.setViewsListed(true)
            defer { model.setViewsListed(false) }
            while !Task.isCancelled {
                try? await Task.sleep(for: .seconds(1))
                // The terminal's own surface went with its screen, or the
                // last lend was refused: the list's is lent again.
                if !Task.isCancelled, model.generation == 0, model.isInFront { model.setViewsListed(true) }
            }
        }
    }

    /// The host's Spaces, projects and threads; a lone Space has no row
    /// of its own, as in the sidebar. Windows no thread claims come last.
    @ViewBuilder
    private var tree: some View {
        if spaces.count == 1, let space = spaces.first {
            ForEach(space.projects) { project in projectRows(space, project, depth: 0) }
        } else {
            ForEach(spaces) { space in
                SidebarRow(icon: "layers", title: space.name, bold: space.current, dim: true) {}
                ForEach(space.projects) { project in projectRows(space, project, depth: 1) }
            }
        }
        ForEach(windows, id: \.id) { window in
            SidebarRow(icon: "app-window", title: window.title.isEmpty ? tr("window.n", window.id) : window.title) {
                model.sideClick("window", id: String(window.id))
                open()
            }
        }
        if spaces.allSatisfy({ $0.projects.isEmpty }) && windows.isEmpty {
            Text(tr("overview.none"))
                .font(.subheadline)
                .foregroundColor(.secondary)
                .padding(.horizontal, 12)
                .padding(.vertical, 10)
        }
    }

    /// The windows the sidebar lists after its tree.
    private var windows: [(id: Int, title: String)] {
        (model.sidebar?.rows ?? []).compactMap { row in
            if case .window(let id, let title, _) = row { return (id, title) }
            return nil
        }
    }

    @ViewBuilder
    private func projectRows(_ space: TreeSpace, _ project: TreeProject, depth: Int) -> some View {
        let key = "project:" + project.id
        let isFolded = folded.contains(key)
        SidebarRow(
            icon: isFolded ? "folder" : "folder-open",
            title: project.name,
            depth: depth,
            dim: true,
            trailing: AnyView(
                Button {
                    model.setSpace(space.id)
                    model.sideClick("new-thread", id: project.id)
                    open()
                } label: {
                    DesktopIcon(name: "circle-plus", size: 18)
                        .foregroundColor(.secondary)
                        .frame(width: 32, height: 32)
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .accessibilityLabel(tr("thread.new"))
            )
        ) {
            withAnimation(.easeOut(duration: 0.18)) {
                if isFolded { folded.remove(key) } else { folded.insert(key) }
            }
        }
        .contextMenu { AppMenuItems(model: model, kind: "project", id: project.id) }
        if !isFolded {
            ForEach(project.threads) { thread in
                // Under the project's name, not stepped in: the desktop's
                // rows all start at one edge.
                threadRow(thread, space: space, depth: depth)
            }
        }
    }

    private func threadRow(_ thread: ThreadRow, space: TreeSpace, depth: Int, subtitle: String? = nil) -> some View {
        SidebarRow(
            lead: AnyView(ThreadCards(core: model.core, icons: thread.icons ?? [], revision: model.iconsRevision)),
            mark: AnyView(ThreadMark(status: thread.markStatus, dot: thread.dot, live: thread.live, size: 9)),
            title: thread.name,
            subtitle: subtitle,
            depth: depth,
            dim: !thread.live
        ) {
            model.openThread(thread.id, space: space.id)
            open()
        }
        .contextMenu { AppMenuItems(model: model, kind: "thread", id: thread.id) }
    }

    /// The threads whose names hold the query, under the host's name, each
    /// with the project it is in; a host with none says nothing.
    @ViewBuilder
    private var found: some View {
        let needle = query.lowercased()
        let hits = spaces.flatMap { space in
            space.projects.flatMap { project in
                project.threads
                    .filter { $0.name.lowercased().contains(needle) }
                    .map { (space: space, project: project, thread: $0) }
            }
        }
        if !hits.isEmpty {
            HostTitle(name: name, color: dot)
            ForEach(hits, id: \.thread.id) { hit in
                threadRow(hit.thread, space: hit.space, depth: 0, subtitle: hit.project.name)
            }
        }
    }
}
