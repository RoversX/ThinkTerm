import Combine
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
    @ObservedObject private var settings = AppSettings.shared
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
    /// The widest the list runs (an iPad's): wider, it keeps to the middle,
    /// as the grouped lists do there. Android's `READABLE_WIDTH`.
    static let readableWidth: CGFloat = 720

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

    /// Populate threads on arrival for known hosts that allow automatic connections.
    private func connectKnownHosts() {
        for entry in entries where sessions.existing(entry.id) == nil {
            if let host = entry.host, !host.autoConnect || host.lastConnected == nil { continue }
            model(entry.id).connect()
        }
    }

    private var byRepo: Bool { settings.byRepo && !searching }

    private var list: some View {
        ScrollView {
            // Not lazy: a host's whole tree is one view in it, which grows
            // and shrinks while the list is off screen (a terminal over it),
            // and a lazy stack came back from one with the scroll's height
            // of before -- the list stuck short of its end. A handful of
            // hosts has no need of laziness.
            VStack(alignment: .leading, spacing: 2) {
                if entries.isEmpty {
                    HostsEmptyState { adding = true }
                        .padding(.top, 40)
                } else {
                    // Every host's projects by the repository they are in,
                    // above the hosts; a search looks host by host as ever.
                    if byRepo {
                        RepoGroups(
                            hosts: entries.compactMap { entry in
                                sessions.existing(entry.id).map { RepoGroups.Host(id: entry.id, name: entry.name, model: $0) }
                            },
                            open: { id in path.append(Route.terminal(id)) }
                        )
                    }
                    ForEach(entries) { entry in
                        HostThreads(
                            name: entry.name,
                            model: sessions.existing(entry.id),
                            query: searching ? query : "",
                            byRepo: byRepo,
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
            .frame(maxWidth: Self.readableWidth)
            .frame(maxWidth: .infinity)
        }
        .scrollDismissesKeyboard(.immediately)
        .foregroundColor(.primary)
        .background(Self.backdrop.ignoresSafeArea())
        // The bars over the list's column, as wide as it is; their ground
        // across the whole window.
        .floatingBar(edge: .top, fading: Self.backdrop, scrim: true) {
            VStack(spacing: 0) {
                header
                if searching { searchField }
            }
            .frame(maxWidth: Self.readableWidth)
            .frame(maxWidth: .infinity)
        }
        .floatingBar(edge: .bottom, fading: Self.backdrop) {
            if !searching {
                footer
                    .frame(maxWidth: Self.readableWidth)
                    .frame(maxWidth: .infinity)
            }
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
            if !searching {
                // Only the threads the Status panel has something in, or
                // every one; kept from one launch to the next. A search
                // looks through them all.
                Button {
                    withAnimation(.snappy(duration: 0.3)) { settings.agentsOnly.toggle() }
                } label: {
                    DesktopIcon(name: "eye")
                        .foregroundColor(settings.agentsOnly ? .white : .primary)
                        .frame(width: 44, height: 44)
                        .contentShape(Circle())
                }
                .buttonStyle(.plain)
                .glassCircle(tint: settings.agentsOnly ? Color.accentColor : nil)
                .accessibilityLabel(tr(settings.agentsOnly ? "home.agents.all" : "home.agents.only"))
                .transition(.opacity.combined(with: .scale(scale: 0.8)))
            }
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
    var byRepo = false
    var connect: () -> Void
    var open: () -> Void
    var connected: (Bool) -> Void

    var body: some View {
        if let model {
            Connected(name: name, model: model, query: query, byRepo: byRepo, connect: connect, open: open, connected: connected)
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
                // A rename starts from the current name, selected.
                renameText = editing.name ?? ""
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
            .selectsOnEditing(renaming == nil || renaming == "new-project" ? nil : renameText)
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
    /// The projects in a repository are listed in theirs, above, and only
    /// the rest here.
    var byRepo = false
    var connect: () -> Void
    var open: () -> Void
    var connected: (Bool) -> Void
    /// Folded projects.
    @State private var folded: Set<String> = []

    private var spaces: [TreeSpace] { model.tree?.spaces ?? [] }

    /// The App keeps the pane's status while its connection is lost. With
    /// the setting off nothing redials it: not ready, not on its way, and
    /// the way back is a tap.
    private var lost: Bool { model.reconnecting && !model.autoReconnect }

    /// Attached to the server: its threads are there to list. The core
    /// says which pane it shows once it is.
    private var ready: Bool { model.connection.hasPrefix("pane ") && !lost }

    /// On the way there: dialling, or connected and attaching.
    private var connecting: Bool {
        !lost && ((model.reconnecting && model.autoReconnect) || (model.isConnected && !ready) || model.connection.contains("reconnect"))
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

    /// Only the threads the Status panel has something in -- an agent, a
    /// program saying how it is doing -- as the eye in the bar asks; a
    /// search looks through every one all the same (`found`).
    private var agentsOnly: Bool { settings.agentsOnly }

    /// A project's threads as the list shows them.
    private func shown(_ project: TreeProject) -> [ThreadRow] {
        agentsOnly ? project.threads.filter { $0.agent == true } : project.threads
    }

    /// A project has its row unless agents only are shown and it has none,
    /// or it is listed in its repository's group.
    private func listed(_ project: TreeProject) -> Bool {
        (!agentsOnly || !shown(project).isEmpty) && !(byRepo && project.repo?.remote != nil)
    }

    /// The host's Spaces, projects and threads; a lone Space has no row
    /// of its own, as in the sidebar. Windows no thread claims come last.
    @ViewBuilder
    private var tree: some View {
        if spaces.count == 1, let space = spaces.first {
            ForEach(space.projects.filter { listed($0) }) { project in projectRows(space, project, depth: 0) }
        } else {
            ForEach(spaces.filter { space in (!agentsOnly && !byRepo) || space.projects.contains { listed($0) } }) { space in
                SidebarRow(icon: "layers", title: space.name, bold: space.current, dim: true) {}
                ForEach(space.projects.filter { listed($0) }) { project in projectRows(space, project, depth: 1) }
            }
        }
        // A window no thread claims stays too when an agent runs in it.
        ForEach(windows.filter { !agentsOnly || $0.agent }, id: \.id) { window in
            SidebarRow(icon: "app-window", title: window.title.isEmpty ? tr("window.n", window.id) : window.title) {
                model.sideClick("window", id: String(window.id))
                open()
            }
        }
        if !spaces.contains(where: { $0.projects.contains { listed($0) } }) && !windows.contains(where: { !agentsOnly || $0.agent }) {
            // Said, not left blank: a host with nothing under it reads as
            // one whose connection broke.
            // Only when the groups do list one of them: with the eye on, a
            // repository's projects without agents are in neither place.
            let above = byRepo && spaces.contains { $0.projects.contains { $0.repo?.remote != nil && (!agentsOnly || !shown($0).isEmpty) } }
            emptyLine(tr(above ? "home.repos.above" : agentsOnly ? "home.agents.none" : "overview.none"))
        }
    }

    private func emptyLine(_ text: String) -> some View {
        Text(text)
            .font(.subheadline)
            .foregroundColor(.secondary)
            .padding(.horizontal, 12)
            .padding(.vertical, 10)
    }

    /// The windows the sidebar lists after its tree.
    private var windows: [(id: Int, title: String, agent: Bool)] {
        (model.sidebar?.rows ?? []).compactMap { row in
            if case .window(let id, let title, _, let agent) = row { return (id, title, agent) }
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
            ForEach(shown(project)) { thread in
                // Under the project's name, not stepped in: the desktop's
                // rows all start at one edge.
                threadRow(thread, space: space, project: project, depth: depth)
            }
        }
    }

    private func threadRow(_ thread: ThreadRow, space: TreeSpace, project: TreeProject, depth: Int, subtitle: String? = nil) -> some View {
        HomeThreadRow(model: model, thread: thread, space: space, project: project, depth: depth, subtitle: subtitle, open: open)
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
                threadRow(hit.thread, space: hit.space, project: hit.project, depth: 0, subtitle: hit.project.name)
            }
        }
    }
}

/// A thread's row: its cards, its name and its mark; a tap opens it, or
/// its agent's chat when chats come first.
private struct HomeThreadRow: View {
    @ObservedObject var model: TerminalModel
    @ObservedObject private var settings = AppSettings.shared
    var thread: ThreadRow
    var space: TreeSpace
    var project: TreeProject
    var depth: Int
    var subtitle: String? = nil
    var open: () -> Void

    var body: some View {
        SidebarRow(
            lead: AnyView(ThreadCards(core: model.core, icons: thread.icons ?? [], revision: model.iconsRevision)),
            mark: AnyView(ThreadMark(status: thread.markStatus, dot: thread.dot, live: thread.live, size: 9)),
            title: thread.name,
            subtitle: subtitle,
            depth: depth,
            dim: !thread.live
        ) {
            if settings.chatFirst, let chat = thread.chat {
                // Its agent's chat, over the terminal of the agent's pane;
                // back from it is back here. The thread is read and recent
                // all the same, as an opened one is.
                if !space.current { model.setSpace(space.id) }
                model.sideClick("touch-thread", id: thread.id)
                model.openChat(pane: chat.pane, agent: chat.agent, place: project.name + " · " + thread.name, fromThreads: true)
            } else {
                model.openThread(thread.id, space: space.id)
            }
            open()
        }
        .contextMenu { AppMenuItems(model: model, kind: "thread", id: thread.id) }
    }
}

/// Every connected host's projects that are in a repository with a remote,
/// put together by repository: its name, then the threads of all its
/// copies. The group is the project, so which computer a thread is on is not
/// said -- unless that computer's connection is down -- and its branch only
/// when the group's threads are on more than one. A host not connected yet
/// has its projects here once it is.
private struct RepoGroups: View {
    struct Host {
        let id: UUID
        let name: String
        let model: TerminalModel
    }

    /// A project of one host in a repository's group: that host's copy.
    struct Copy: Identifiable {
        let id: String
        let host: Host
        let space: TreeSpace
        let project: TreeProject
        let branch: String?
    }

    var hosts: [Host]
    var open: (UUID) -> Void
    @ObservedObject private var settings = AppSettings.shared
    /// Moves on as any host's tree or connection changes, which this view
    /// does not observe one by one: their trees are read again.
    @StateObject private var watch = RepoWatch()

    private struct Repo: Identifiable {
        let id: String
        let name: String
        /// Under its name: its owner, not where it is hosted, unless asked
        /// for or the host is what tells it from another.
        let under: String?
        let copies: [Copy]
    }

    /// A thread of one of a repository's copies, as the group lists it.
    private struct Line: Identifiable {
        let id: String
        let copy: Copy
        let thread: ThreadRow
    }

    /// Attached to its server, as `Connected` reads it: its tree is the
    /// one the server has now.
    private func ready(_ model: TerminalModel) -> Bool {
        model.connection.hasPrefix("pane ") && !(model.reconnecting && !model.autoReconnect)
    }

    private var groups: [Repo] {
        watch.watch(hosts.map(\.model))
        _ = watch.tick
        let live = hosts.filter { ready($0.model) }
        var projects: [RepoProject] = []
        for host in live {
            for space in host.model.tree?.spaces ?? [] {
                for project in space.projects {
                    guard let repo = project.repo, let remote = repo.remote else { continue }
                    projects.append(RepoProject(
                        host: host.id.uuidString, space: space.id, project: project.id,
                        remote: remote, branch: repo.branch, worktree: repo.worktree
                    ))
                }
            }
        }
        return repoGroups(projects: projects).map { group in
            let copies: [Copy] = group.projects.compactMap { at in
                guard let host = live.first(where: { $0.id.uuidString == at.host }),
                      let space = host.model.tree?.spaces.first(where: { $0.id == at.space }),
                      let project = space.projects.first(where: { $0.id == at.project })
                else { return nil }
                return Copy(id: at.host + "/" + at.project, host: host, space: space, project: project, branch: at.branch)
            }
            let under = settings.repoHost || group.hostNeeded ? group.remote : (group.owner.isEmpty ? nil : group.owner)
            return Repo(id: group.remote.lowercased(), name: group.name, under: under, copies: copies)
        }
    }

    /// A project's threads as the list shows them: with the eye on, only
    /// the ones the Status panel has something in.
    private func shown(_ project: TreeProject) -> [ThreadRow] {
        settings.agentsOnly ? project.threads.filter { $0.agent == true } : project.threads
    }

    /// The line under a thread: its computer only while that one's
    /// connection is down, its branch only when the group's threads are on
    /// more than one.
    private func subtitle(_ copy: Copy, sayBranch: Bool) -> String? {
        let parts = [
            copy.host.model.reconnecting ? copy.host.name + " " + tr("offline") : nil,
            sayBranch ? copy.branch : nil,
        ].compactMap { $0 }
        return parts.isEmpty ? nil : parts.joined(separator: " · ")
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            ForEach(groups) { group in
                let lines = group.copies.flatMap { copy in
                    shown(copy.project).map { Line(id: copy.host.id.uuidString + "/" + $0.id, copy: copy, thread: $0) }
                }
                if !settings.agentsOnly || !lines.isEmpty {
                    let sayBranch = Set(group.copies.filter { !shown($0.project).isEmpty }.map { $0.branch }).count > 1
                    RepoTitle(name: group.name, under: group.under, copies: group.copies, open: open)
                    ForEach(lines) { line in
                        // Under the repository's name, not stepped in: the
                        // desktop's rows all start at one edge.
                        HomeThreadRow(
                            model: line.copy.host.model,
                            thread: line.thread,
                            space: line.copy.space,
                            project: line.copy.project,
                            depth: 0,
                            subtitle: subtitle(line.copy, sayBranch: sayBranch),
                            open: { open(line.copy.host.id) }
                        )
                    }
                }
            }
        }
    }
}

/// What `RepoGroups` is made of, watched: the hosts' trees and connections,
/// and only as they change -- a model publishes much else, its screen's
/// every change among it. Subscribed once for a set of hosts, not as the
/// view is drawn, so no change falls between two subscriptions.
private final class RepoWatch: ObservableObject {
    @Published private(set) var tick = 0
    private var watched: [ObjectIdentifier] = []
    private var subscription: AnyCancellable?

    func watch(_ models: [TerminalModel]) {
        let ids = models.map(ObjectIdentifier.init)
        guard ids != watched else { return }
        watched = ids
        // A `@Published` sends what it holds as it is subscribed to: that
        // is what was just drawn, not a change.
        subscription = Publishers.MergeMany(models.flatMap { model -> [AnyPublisher<Void, Never>] in
            [
                model.$tree.removeDuplicates().dropFirst().map { _ in () }.eraseToAnyPublisher(),
                model.$connection.removeDuplicates().dropFirst().map { _ in () }.eraseToAnyPublisher(),
                model.$reconnecting.removeDuplicates().dropFirst().map { _ in () }.eraseToAnyPublisher(),
            ]
        })
        .receive(on: RunLoop.main)
        .sink { [weak self] in self?.tick &+= 1 }
    }
}

/// A repository's name, the group's root, with whose it is under it, and
/// the + that starts a thread in it: in its one copy, or in the one picked
/// from a menu -- the only place a computer is named.
private struct RepoTitle: View {
    var name: String
    var under: String?
    var copies: [RepoGroups.Copy]
    var open: (UUID) -> Void

    var body: some View {
        HStack(spacing: 8) {
            VStack(alignment: .leading, spacing: 1) {
                Text(name)
                    .font(.title3.weight(.bold))
                    .lineLimit(1)
                if let under {
                    Text(under)
                        .font(.caption)
                        .foregroundColor(.secondary)
                        .lineLimit(1)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            if copies.count == 1, let copy = copies.first {
                Button { start(copy) } label: { plus }
                    .buttonStyle(.plain)
                    .accessibilityLabel(tr("thread.new"))
            } else {
                Menu {
                    ForEach(copies) { copy in
                        Button([copy.host.name, copy.branch].compactMap { $0 }.joined(separator: " · ")) { start(copy) }
                    }
                } label: { plus }
                    .accessibilityLabel(tr("thread.new"))
            }
        }
        .padding(.horizontal, 12)
        .padding(.top, 18)
        .padding(.bottom, 4)
    }

    private var plus: some View {
        DesktopIcon(name: "circle-plus", size: 18)
            .foregroundColor(.secondary)
            .frame(width: 32, height: 32)
            .contentShape(Rectangle())
    }

    private func start(_ copy: RepoGroups.Copy) {
        copy.host.model.setSpace(copy.space.id)
        copy.host.model.sideClick("new-thread", id: copy.project.id)
        open(copy.host.id)
    }
}
