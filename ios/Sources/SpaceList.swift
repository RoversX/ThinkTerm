import Combine
import SwiftUI

/// Where a Space list is: Threads home, the whole screen; or a terminal's
/// sidebar, beside it or under it.
enum ListPlace {
    case home
    case panel
}

/// A host the lists name: a saved one, or in debug builds the probe on
/// this Mac, which has no row among the saved ones (its session is kept
/// under `Sessions.probeId`).
struct ListedHost: Identifiable {
    let id: UUID
    let name: String
    let host: Host?
}

/// The hosts the lists name, in the hosts list's order.
func listedHosts(_ store: HostStore) -> [ListedHost] {
    var out = store.hosts.map { ListedHost(id: $0.id, name: $0.display, host: $0) }
    #if DEBUG
    if probeIsUp {
        out.append(ListedHost(id: Sessions.probeId, name: tr("thismac"), host: nil))
    }
    #endif
    return out
}

#if DEBUG
/// The probe's sshd left its key: `ios/tools/probe.sh up` has run.
private let probeIsUp = FileManager.default.fileExists(atPath: "/tmp/ttp-ssh/userkey")
#endif

/// The host whose Space the lists show: the one looked at last (a Space
/// of it picked, its terminal shown), else one whose connection is up,
/// else the first.
func shownHost(_ entries: [ListedHost], sessions: Sessions) -> ListedHost? {
    let last = AppSettings.shared.spaceHost
    return entries.first { $0.id.uuidString == last }
        ?? entries.first { sessions.existing($0.id)?.listReady == true }
        ?? entries.first
}

extension TerminalModel {
    /// The host's tree is the one its server has now: attached (the core
    /// says which pane it shows), and its connection not lost for good --
    /// with auto-reconnect off nothing redials it.
    var listReady: Bool { connection.hasPrefix("pane ") && !(reconnecting && !autoReconnect) }

    /// Up, and not lost for good: its Spaces are there to go to.
    var connectionUp: Bool { isConnected && !(reconnecting && !autoReconnect) }

    /// On its way: dialling, redialling, or connected and not attached yet.
    var onItsWay: Bool {
        let lost = reconnecting && !autoReconnect
        return pendingHostKey == nil && !lost
            && ((reconnecting && autoReconnect) || (isConnected && !listReady) || connection.contains("reconnect"))
    }

    /// The Space the lists show of the host: the one on show in its App.
    var shownSpace: TreeSpace? {
        guard let spaces = tree?.spaces else { return nil }
        return spaces.first(where: \.current) ?? spaces.first
    }
}

/// The threads a list shows: with the eye on, only the ones the Status
/// panel has something in -- an agent, a program saying how it is doing.
private func shownThread(_ thread: ThreadRow, agentsOnly: Bool) -> Bool {
    !agentsOnly || thread.agent == true
}

/// A project has its row unless agents only are shown and it has none.
private func listedProject(_ project: TreeProject, agentsOnly: Bool) -> Bool {
    !agentsOnly || project.threads.contains { $0.agent == true }
}

/// A Space a swipe goes to: its host's id, and its own.
private struct SpaceAt: Equatable {
    let host: UUID
    let space: String
}

/// Every Space a swipe goes through, in the order the Space menu lists
/// them: host by host, the hosts whose connection is up. (One whose views
/// no list reads now still has its tree from when one did.)
private func spaceOrder(_ entries: [ListedHost], sessions: Sessions) -> [SpaceAt] {
    entries.flatMap { entry -> [SpaceAt] in
        guard let model = sessions.existing(entry.id), model.connectionUp else { return [] }
        return (model.tree?.spaces ?? []).map { SpaceAt(host: entry.id, space: $0.id) }
    }
}

/// A listed host with a session: its id, its name, and the session.
private struct HostSession: Identifiable {
    let id: UUID
    let name: String
    let model: TerminalModel
}

/// The threads of one Space -- of any host, picked at the list's head as
/// the desktop's Space menu picks one, or swiped to -- its pinned threads,
/// its projects and their threads, and what the desktop lists after them.
/// Threads home is this list, the whole screen; a terminal's sidebar is
/// this list again, beside the terminal or under it. A search looks
/// through every host's threads.
struct SpaceList: View {
    var place: ListPlace
    /// A sidebar: its terminal's host, whose Space it shows. Threads home
    /// shows the one looked at last (`shownHost`).
    var host: UUID? = nil
    /// A thread (or its chat, or a new one) is to be shown on the host with
    /// this id, its session told already: that host's terminal comes up.
    /// `keepPanel`: a Space of it was picked in a sidebar, and the sidebar
    /// comes out with the terminal.
    var onOpen: (_ host: UUID, _ keepPanel: Bool) -> Void
    var onSettings: () -> Void
    /// Threads home: the list of hosts, and a new one when there is none.
    var onHosts: (() -> Void)? = nil
    var onAddHost: (() -> Void)? = nil
    /// A sidebar: back to Threads home.
    var onBack: (() -> Void)? = nil
    /// A sidebar: the host's editor.
    var onEditHost: (() -> Void)? = nil
    /// A sidebar: the overview's row, lit while it is up, and its tap.
    var overview: (on: Bool, tap: () -> Void)? = nil
    /// A sidebar's drawer closes as something is picked in it, and as it
    /// is swiped away with no Space that way to go to.
    var done: () -> Void = {}
    /// The terminal on screen's own session: it reads its views already,
    /// and its screen asks what its App asks.
    var inFront: TerminalModel? = nil
    /// Someone sees it. A drawer shut under the screen makes no rows, and
    /// keeps its folds and search for when it opens; Threads home under a
    /// page pushed over it keeps its rows and asks nothing.
    var live = true

    @EnvironmentObject private var store: HostStore
    @EnvironmentObject private var sessions: Sessions
    @ObservedObject private var lang = AppLanguage.shared
    @ObservedObject private var settings = AppSettings.shared
    /// Moves on as any host's tree or connection changes: the head, the
    /// menu and the swipe read every host's.
    @StateObject private var watch = TreesWatch()
    @State private var searching = false
    @State private var search = ThreadSearch()
    /// Folded projects, by id, the list over.
    @State private var folded: Set<String> = []
    /// Where the system puts bars along the top and bottom (`barRoom`):
    /// on some screens further in than the safe area's edges.
    @State private var headerRoom = EdgeInsets()
    @State private var footerRoom = EdgeInsets()
    /// How far the list follows a sideways swipe, and that swipe.
    @GestureState(resetTransaction: Transaction(animation: .spring(response: 0.35, dampingFraction: 0.8)))
    private var drag: CGFloat = 0
    @State private var swipe = SpaceSwipe()
    /// The Space gone to lies after the one left: it comes in from the
    /// right.
    @State private var forward = true

    private static let backdrop = Color.chromeBackdrop
    /// The header's row and the footer's, their padding included.
    private static let headerHeight: CGFloat = 60
    private static let footerHeight: CGFloat = 64
    /// How far a sideways swipe goes before it goes to another Space.
    private static let swipeAt: CGFloat = 64
    /// How far a Space comes in from the side, and the one left goes.
    private static let slideBy: CGFloat = 120

    var body: some View {
        let entries = listedHosts(store)
        let made = live || place == .home
        let _ = watch.watch(made ? entries.compactMap { sessions.existing($0.id) } : [])
        Group {
            if made {
                list(entries)
            } else {
                Color.clear
            }
        }
        .foregroundColor(.primary)
        .background(Self.backdrop.ignoresSafeArea())
    }

    /// The host on show: the sidebar's own, or the one looked at last.
    private func shownEntry(_ entries: [ListedHost]) -> ListedHost? {
        guard let host else { return shownHost(entries, sessions: sessions) }
        return entries.first { $0.id == host }
            ?? ListedHost(id: host, name: inFront?.host?.display ?? tr("thismac"), host: inFront?.host)
    }

    /// The list runs the whole height, under the buttons at the top and
    /// those at the bottom, blurring and fading as it goes under them. The
    /// Space's name is its head, and goes under with it.
    private func list(_ entries: [ListedHost]) -> some View {
        let shown = shownEntry(entries)
        let model = shown.flatMap { sessions.existing($0.id) }
        let spaceId = model?.shownSpace?.id
        let key = (shown?.id.uuidString ?? "") + "/" + (spaceId ?? "")
        let order = spaceOrder(entries, sessions: sessions)
        let at = order.firstIndex { $0.host == shown?.id && $0.space == spaceId }
        return ScrollView {
            // Not lazy: a lazy stack came back from a terminal over it with
            // the scroll's height of before, the list stuck short of its
            // end. A Space's rows have no need of laziness.
            ZStack(alignment: .top) {
                content(entries, shown: shown, model: model, spaceId: spaceId, order: order, at: at)
                    .id(key)
                    .transition(slide)
            }
            .offset(x: drag)
            .animation(.easeOut(duration: 0.3), value: key)
        }
        .scrollDismissesKeyboard(.immediately)
        // Swiped sideways, the list goes to the Space before or after the
        // one on show, as the desktop's sidebar does. A search stays put.
        .simultaneousGesture(swipeGesture(entries, shown: shown, order: order, at: at), including: searching ? .subviews : .all)
        .floatingBar(edge: .top, fading: Self.backdrop, scrim: true) {
            VStack(spacing: 0) {
                header
                if searching { searchField }
            }
            .readable(place == .home)
        }
        .floatingBar(edge: .bottom, fading: Self.backdrop) {
            if !searching {
                footer(shown, model)
                    .readable(place == .home)
            }
        }
        .barRoom(.top, height: Self.headerHeight, $headerRoom)
        .barRoom(.bottom, height: Self.footerHeight, $footerRoom)
        .background { chores(entries, shown: shown) }
    }

    /// The Space after the one left comes in from the right, the one left
    /// going out to the left; back, the other way.
    private var slide: AnyTransition {
        .asymmetric(
            insertion: .offset(x: forward ? Self.slideBy : -Self.slideBy).combined(with: .opacity),
            removal: .offset(x: forward ? -Self.slideBy : Self.slideBy).combined(with: .opacity)
        )
    }

    /// The head and the rows of the Space on show; a search's finds in
    /// their place.
    private func content(
        _ entries: [ListedHost], shown: ListedHost?, model: TerminalModel?, spaceId: String?, order: [SpaceAt], at: Int?
    ) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            if let shown {
                SpaceTitle(
                    look: look(entries, shown: shown, model: model, spaceId: spaceId),
                    model: model,
                    pick: { id, space in pick(entries.first { $0.id == id }, space, shown: shown, order: order, at: at) },
                    editHost: onEditHost
                )
                .equatable()
                if searching {
                    Found(search: search, hosts: sessionsOf(entries), open: openRow)
                } else {
                    // Every host's projects by the repository they are in,
                    // above the Space's own.
                    if settings.byRepo {
                        RepoGroups(hosts: sessionsOf(entries), open: { started(on: $0) }, openRow: openRow)
                    }
                    if let model {
                        SpaceRows(
                            host: shown.id,
                            model: model,
                            spaceId: spaceId,
                            folded: $folded,
                            skipGrouped: settings.byRepo,
                            overview: overview,
                            openHost: {
                                done()
                                onOpen(shown.id, false)
                            },
                            openRow: openRow
                        )
                    } else {
                        SidebarRow(icon: "link-2", title: tr("connect"), dim: true) {
                            pick(shown, nil, shown: shown, order: order, at: at)
                        }
                    }
                }
            } else {
                HostsEmptyState { onAddHost?() }
                    .padding(.top, 40)
            }
        }
        .padding(.horizontal, 10)
        .padding(.bottom, 12)
        .readable(place == .home)
    }

    /// What the head and its menu show, as values: the head is drawn again
    /// only when one of them changes.
    private func look(_ entries: [ListedHost], shown: ListedHost, model: TerminalModel?, spaceId: String?) -> SpaceTitle.Look {
        let space = model?.tree?.spaces.first { $0.id == spaceId }
        return SpaceTitle.Look(
            home: place == .home,
            host: shown.id,
            title: space?.name ?? shown.name,
            hostName: shown.name,
            dot: HostDot(model),
            busy: model?.onItsWay ?? false,
            ready: model?.listReady ?? false,
            up: model?.connectionUp ?? false,
            editable: onEditHost != nil && shown.host != nil,
            session: model.map { ObjectIdentifier($0) },
            hosts: entries.map { entry -> MenuHost in
                let session = sessions.existing(entry.id)
                return MenuHost(
                    id: entry.id,
                    name: entry.name,
                    dot: HostDot(session),
                    busy: session?.onItsWay ?? false,
                    spaces: session.flatMap { listed -> [MenuSpace]? in
                        guard listed.listReady else { return nil }
                        return listed.tree?.spaces.map { MenuSpace(id: $0.id, name: $0.name, current: $0.current) }
                    }
                )
            }
        )
    }

    private func sessionsOf(_ entries: [ListedHost]) -> [HostSession] {
        entries.compactMap { entry in
            sessions.existing(entry.id).map { HostSession(id: entry.id, name: entry.name, model: $0) }
        }
    }

    /// Shows a Space of a host -- or, with none, connects to the host --
    /// and keeps it as the one the lists show. A sidebar's terminal goes
    /// with its Space to another host.
    private func pick(_ to: ListedHost?, _ spaceId: String?, shown: ListedHost?, order: [SpaceAt], at: Int?) {
        guard let to else { return }
        if let at, let next = order.firstIndex(where: { $0.host == to.id && $0.space == spaceId }) {
            forward = next >= at
        } else {
            forward = true
        }
        let session = sessions.model(for: to.host, store: store)
        if let spaceId {
            session.setSpace(spaceId)
        } else if !session.connectionUp {
            // Not up, or lost and not redialing by itself: dialled again.
            session.connect()
        }
        settings.spaceHost = to.id.uuidString
        if place == .panel, to.id != shown?.id { onOpen(to.id, true) }
    }

    /// A thread's row tapped, wherever it is listed: its agent's chat when
    /// chats come first, else the thread; then its host's terminal.
    private func openRow(_ on: UUID, _ model: TerminalModel, _ space: TreeSpace, _ project: TreeProject, _ thread: ThreadRow) {
        if settings.chatFirst, let chat = thread.chat {
            // Its agent's chat, over the terminal of the agent's pane. From
            // Threads home, back from it is back there. The thread is read
            // and recent all the same, as an opened one is.
            if !space.current { model.setSpace(space.id) }
            model.sideClick("touch-thread", id: thread.id)
            model.openChat(pane: chat.pane, agent: chat.agent, place: project.name + " · " + thread.name, fromThreads: place == .home)
        } else {
            model.openThread(thread.id, space: space.id)
        }
        settings.spaceHost = on.uuidString
        // A sidebar's search has found what it was for.
        if place == .panel, searching {
            searching = false
            search.query = ""
        }
        done()
        onOpen(on, false)
    }

    /// A thread started in a repository's group, on the host with `id`.
    private func started(on id: UUID) {
        settings.spaceHost = id.uuidString
        done()
        onOpen(id, false)
    }

    /// Decided as the touch sets off, plainly sideways (twice as far as up
    /// or down), so a scroll stays a scroll. The list follows the finger a
    /// third of the way and springs back; past `swipeAt` the Space before or
    /// after the one on show comes. A drawer with no Space that way to go
    /// to is swiped away instead.
    private func swipeGesture(_ entries: [ListedHost], shown: ListedHost?, order: [SpaceAt], at: Int?) -> some Gesture {
        DragGesture(minimumDistance: 16)
            // Gesture state: back to rest however the swipe ends, cancelled
            // by the system too, not only when let go.
            .updating($drag) { value, offset, _ in
                if swipe.start != value.startLocation {
                    let move = value.translation
                    swipe.start = value.startLocation
                    swipe.sideways = abs(move.width) >= abs(move.height) * 2
                    swipe.follows = swipe.sideways && order.count > 1 && at != nil
                }
                if swipe.follows { offset = value.translation.width / 3 }
            }
            .onEnded { value in
                let dx = value.translation.width
                let follows = swipe.follows
                let sideways = swipe.sideways
                swipe.start = nil
                guard follows, let at else {
                    if sideways, dx <= -Self.swipeAt { done() }
                    return
                }
                let to: Int? = dx <= -Self.swipeAt ? at + 1 : dx >= Self.swipeAt ? at - 1 : nil
                guard let to, order.indices.contains(to) else {
                    if dx <= -Self.swipeAt { done() }
                    return
                }
                let target = order[to]
                pick(entries.first { $0.id == target.host }, target.space, shown: shown, order: order, at: at)
            }
    }

    // MARK: the bars

    /// The way back (a sidebar's), and the eye and the search, floating
    /// over the list: what scrolls under them shows through, fading there.
    private var header: some View {
        HStack(spacing: 10) {
            if let onBack {
                Button {
                    done()
                    onBack()
                } label: {
                    DesktopIcon(name: "chevron-left")
                        .frame(width: 44, height: 44)
                        .contentShape(Circle())
                }
                .buttonStyle(.plain)
                .glassCircle()
                .accessibilityLabel(tr("threads"))
            }
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
                    if !searching { search.query = "" }
                }
                // The field takes the keyboard as it comes up, not each
                // time the list makes it again.
                search.focusNext = searching
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
        .padding(.leading, max(14, headerRoom.leading))
        .padding(.trailing, 14)
        .padding(.top, 6 + max(0, headerRoom.top))
        .padding(.bottom, 8)
    }

    private var searchField: some View {
        HStack(spacing: 8) {
            DesktopIcon(name: "search", size: 18).foregroundColor(.secondary)
            SearchField(search: search, prompt: tr("thread.find"))
        }
        .foregroundColor(.primary)
        .padding(.horizontal, 14)
        .frame(height: 40)
        .glassCapsule()
        .padding(.horizontal, 14)
        .padding(.bottom, 8)
        .transition(.move(edge: .top).combined(with: .opacity))
    }

    /// A new thread in the Space on show, where the thumb is, then the
    /// hosts (Threads home's) and the settings. Searching puts them away.
    private func footer(_ shown: ListedHost?, _ model: TerminalModel?) -> some View {
        HStack(spacing: 12) {
            if let shown, let model, model.listReady {
                Button {
                    model.sideClick("new-thread")
                    settings.spaceHost = shown.id.uuidString
                    done()
                    onOpen(shown.id, false)
                } label: {
                    HStack(spacing: 8) {
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
                }
                .buttonStyle(.plain)
                .glassCapsule(tint: .accentColor)
            }
            Spacer(minLength: 0)
            HStack(spacing: 10) {
                if let onHosts {
                    roundButton("server", label: tr("hosts"), action: onHosts)
                }
                roundButton("settings", label: tr("settings")) {
                    done()
                    onSettings()
                }
            }
            .glassGroup(spacing: 10)
        }
        .foregroundColor(.primary)
        .dynamicTypeSize(...DynamicTypeSize.xxLarge)
        .padding(.leading, max(14, footerRoom.leading))
        .padding(.trailing, 14)
        .padding(.top, 6)
        // Below the safe area's edge where the system's bars go there.
        .padding(.bottom, 8 + footerRoom.bottom)
    }

    private func roundButton(_ icon: String, label: String, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            DesktopIcon(name: icon, size: 22)
                .frame(width: 50, height: 50)
                .contentShape(Circle())
        }
        .buttonStyle(.plain)
        .glassCircle()
        .accessibilityLabel(label)
    }

    /// What the list does for each host's session besides its rows: reads
    /// its views while it shows them, and asks what its App asks from a
    /// row's menu. Threads home reads every host's, its rows on the screen
    /// or not, as the Space menu and a search name them all; a sidebar reads
    /// the others' only while it looks at them (a search, the repositories),
    /// the terminal on show reading its own and asking for itself.
    @ViewBuilder
    private func chores(_ entries: [ListedHost], shown: ListedHost?) -> some View {
        let readAll = place == .home || searching || settings.byRepo
        ForEach(entries) { entry in
            if let listed = sessions.existing(entry.id), listed !== inFront {
                // A session closed and made again is another one to read.
                ListChores(model: listed, reads: readAll || entry.id == shown?.id, asks: live)
                    .id(ObjectIdentifier(listed))
            }
        }
    }
}

/// Where the sideways swipe under way set off, and what was decided as it
/// did: whether it goes sideways, and whether the list follows it.
private final class SpaceSwipe {
    var start: CGPoint?
    var sideways = false
    var follows = false
}

private extension View {
    /// Threads home on a wide screen keeps to a readable width, in the
    /// middle, as the system's lists do on an iPad.
    func readable(_ on: Bool) -> some View {
        frame(maxWidth: on ? ThreadsHome.readableWidth : .infinity)
            .frame(maxWidth: .infinity)
    }
}

// MARK: the head

/// A host's connection, as its dot says it: up, waiting on the person (a
/// key to trust), failed, or neither.
private enum HostDot: Equatable {
    case up
    case waiting
    case failed
    case off

    init(_ model: TerminalModel?) {
        guard let model else {
            self = .off
            return
        }
        if model.listReady {
            self = .up
        } else if model.pendingHostKey != nil {
            self = .waiting
        } else if model.connection.hasPrefix("failed") {
            self = .failed
        } else {
            self = .off
        }
    }

    var color: Color {
        switch self {
        case .up: return .green
        case .waiting: return .orange
        case .failed: return .red
        case .off: return Color.primary.opacity(0.25)
        }
    }

    /// The dot as a picture a menu shows in its own colour: a menu draws
    /// the pictures it is given in its text's colour otherwise.
    var picture: UIImage {
        let ink: UIColor
        switch self {
        case .up: ink = .systemGreen
        case .waiting: ink = .systemOrange
        case .failed: ink = .systemRed
        case .off: ink = .systemGray
        }
        return UIGraphicsImageRenderer(size: CGSize(width: 16, height: 16)).image { _ in
            ink.setFill()
            UIBezierPath(ovalIn: CGRect(x: 4, y: 4, width: 8, height: 8)).fill()
        }
        .withRenderingMode(.alwaysOriginal)
    }
}

/// A host as the Space menu lists it: its name, how its connection stands,
/// and its Spaces once its tree is the server's.
private struct MenuHost: Equatable, Identifiable {
    let id: UUID
    let name: String
    let dot: HostDot
    let busy: Bool
    let spaces: [MenuSpace]?
}

private struct MenuSpace: Equatable, Identifiable {
    let id: String
    let name: String
    let current: Bool
}

/// The Space on show, the list's head as on the desktop's sidebar: its
/// name large, the host it is on under it, and a tap away the Space menu --
/// every host's Spaces, host by host, and what the one on show offers.
private struct SpaceTitle: View, Equatable {
    /// What the head shows and its menu holds: the head is drawn again only
    /// when one of these changes, not with every change of the list round
    /// it, its menu open or not.
    struct Look: Equatable {
        var home: Bool
        var host: UUID
        var title: String
        var hostName: String
        var dot: HostDot
        var busy: Bool
        var ready: Bool
        var up: Bool
        var editable: Bool
        /// The session it acts on: one closed and made anew is another.
        var session: ObjectIdentifier?
        var hosts: [MenuHost]
    }

    var look: Look
    var model: TerminalModel?
    var pick: (UUID, String?) -> Void
    var editHost: (() -> Void)?

    static func == (a: SpaceTitle, b: SpaceTitle) -> Bool { a.look == b.look }

    var body: some View {
        HStack(spacing: 0) {
            Menu {
                menu
            } label: {
                label
            }
            .buttonStyle(.plain)
            Spacer(minLength: 0)
        }
        .padding(.top, 4)
        .padding(.bottom, 8)
    }

    private var label: some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack(spacing: 6) {
                Text(look.title)
                    .font(look.home ? Font.title.weight(.bold) : Font.title2.weight(.bold))
                    .foregroundColor(.primary)
                    .lineLimit(1)
                DesktopIcon(name: "chevrons-up-down", size: 16)
                    .foregroundColor(.secondary)
            }
            HStack(spacing: 6) {
                if look.busy {
                    Spinner(color: .orange).frame(width: 10, height: 10)
                } else {
                    Circle().fill(look.dot.color).frame(width: 8, height: 8)
                }
                Text(look.hostName)
                    .font(.subheadline)
                    .foregroundColor(.secondary)
                    .lineLimit(1)
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 4)
        .contentShape(Rectangle())
        .accessibilityElement(children: .combine)
    }

    /// Each host and its Spaces (the one on show ticked), a host not
    /// connected offering to connect; then what the Space on show offers,
    /// as its App words it -- a new Space, its own new name, its end -- and
    /// the host's connection.
    @ViewBuilder
    private var menu: some View {
        ForEach(look.hosts) { entry in
            Section {
                // The host's name and its connection's dot: a heading, not
                // a row to pick.
                Label {
                    Text(entry.name)
                } icon: {
                    Image(uiImage: entry.dot.picture)
                }
                if let spaces = entry.spaces {
                    ForEach(spaces) { space in
                        Button {
                            pick(entry.id, space.id)
                        } label: {
                            if entry.id == look.host && space.current {
                                Label(space.name, systemImage: "checkmark")
                            } else {
                                Text(space.name)
                            }
                        }
                    }
                } else {
                    Button(tr(entry.busy ? "connecting" : "connect")) { pick(entry.id, nil) }
                }
            }
        }
        if let model {
            Section {
                if look.ready { SpaceOwnItems(model: model) }
                Button(look.up ? tr("disconnect") : tr("reconnect")) {
                    if look.up { model.disconnect() } else { model.connect() }
                }
                if look.editable, let editHost {
                    Button(tr("edithost"), action: editHost)
                }
            }
        }
    }
}

/// The App's own items for the Space on show, its Spaces aside: the Space
/// menu lists them above, with every other host's.
private struct SpaceOwnItems: View {
    let model: TerminalModel

    var body: some View {
        let items = model.contextMenu("space", id: "").filter { !$0.id.hasPrefix("space:") && $0.kind != "separator" }
        ForEach(items, id: \.rowId) { item in
            AppMenuItems.row(item, model: model)
        }
    }
}

// MARK: the rows

/// The Space's rows: the overview (a sidebar's), its pinned threads, its
/// projects and their threads, and what the desktop lists after them --
/// archived projects, windows no thread claims. A host not attached says
/// how its connection stands instead.
private struct SpaceRows: View {
    var host: UUID
    @ObservedObject var model: TerminalModel
    @ObservedObject private var settings = AppSettings.shared
    /// The Space the list was drawn for: one on its way out keeps its own.
    var spaceId: String?
    @Binding var folded: Set<String>
    /// With projects by repository, the ones in a repository are listed in
    /// theirs, above, and only the rest here.
    var skipGrouped: Bool
    var overview: (on: Bool, tap: () -> Void)?
    /// Something here is to be shown on the host's terminal, its session
    /// told already.
    var openHost: () -> Void
    var openRow: (UUID, TerminalModel, TreeSpace, TreeProject, ThreadRow) -> Void

    private var agentsOnly: Bool { settings.agentsOnly }

    var body: some View {
        // One view, not a group of rows: the list's spacing, kept inside it.
        VStack(alignment: .leading, spacing: 2) {
            if model.pendingHostKey != nil {
                // The key is shown, and trusted or not, on its screen.
                SidebarRow(icon: "key-round", title: tr("hostkey.approve.title"), bold: true, action: openHost)
            } else if !model.listReady {
                if model.onItsWay {
                    SidebarRow(icon: "link-2", title: tr("connecting"), dim: true) {}
                } else {
                    SidebarRow(icon: "rotate-ccw", title: tr("reconnect"), dim: true) { model.connect() }
                }
            } else {
                if let overview {
                    // The screen shuts the drawer itself, and waits for it.
                    SidebarRow(icon: "grid-2x2", title: tr("overview"), selected: overview.on, action: overview.tap)
                }
                if let space = model.tree?.spaces.first(where: { $0.id == spaceId }) ?? model.shownSpace {
                    spaceRows(space)
                }
            }
        }
    }

    private func listedHere(_ project: TreeProject) -> Bool {
        listedProject(project, agentsOnly: agentsOnly) && !(skipGrouped && project.repo?.remote != nil)
    }

    @ViewBuilder
    private func spaceRows(_ space: TreeSpace) -> some View {
        // As the desktop pins them: they leave their projects' lists. A
        // repository's are in its group, above, with the rest of its threads.
        let pinned = space.projects.filter { !(skipGrouped && $0.repo?.remote != nil) }.flatMap { project in
            project.threads
                .filter { $0.pinned && shownThread($0, agentsOnly: agentsOnly) }
                .map { (project: project, thread: $0) }
        }
        if !pinned.isEmpty {
            ListSection(text: tr("pinned"))
            ForEach(pinned, id: \.thread.id) { entry in
                ThreadItem(model: model, thread: entry.thread, subtitle: entry.project.name) {
                    openRow(host, model, space, entry.project, entry.thread)
                }
            }
        }
        HStack {
            ListSection(text: tr("workspaces"))
            Spacer()
            Button {
                model.sideClick("new-project")
                model.askName()
            } label: {
                DesktopIcon(name: "project-add", size: 18)
                    .foregroundColor(.secondary)
                    .frame(width: 36, height: 36)
                    .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .accessibilityLabel(tr("project.new"))
        }
        ForEach(space.projects.filter { listedHere($0) }) { project in
            projectRows(space, project)
        }
        tailRows
        if let error = model.sidebar?.new_project_error {
            Text(error).font(.footnote).foregroundColor(.red).padding(.horizontal, 12).padding(.vertical, 8)
        }
        if !space.projects.contains(where: { listedHere($0) }) && !windowsShown {
            // Said, not left blank: a Space with nothing in it reads as one
            // whose connection broke.
            Text(tr(emptyLine(space)))
                .font(.subheadline)
                .foregroundColor(.secondary)
                .padding(.horizontal, 12)
                .padding(.vertical, 10)
        }
    }

    /// What an empty Space says. That its projects are above only when the
    /// groups do list one of them: with the eye on, a repository's projects
    /// without agents are in neither place.
    private func emptyLine(_ space: TreeSpace) -> String {
        let above = skipGrouped && space.projects.contains { $0.repo?.remote != nil && listedProject($0, agentsOnly: agentsOnly) }
        return above ? "home.repos.above" : agentsOnly ? "home.agents.none" : "overview.none"
    }

    @ViewBuilder
    private func projectRows(_ space: TreeSpace, _ project: TreeProject) -> some View {
        let key = "project:" + project.id
        let isFolded = folded.contains(key)
        SidebarRow(
            icon: isFolded ? "folder" : "folder-open",
            title: project.name,
            dim: true,
            trailing: AnyView(
                Button {
                    model.setSpace(space.id)
                    model.sideClick("new-thread", id: project.id)
                    openHost()
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
            // Under the project's name, not stepped in: the desktop's rows
            // all start at one edge. Pinned threads are above, in their own
            // list.
            ForEach(project.threads.filter { !$0.pinned && shownThread($0, agentsOnly: agentsOnly) }) { thread in
                ThreadItem(model: model, thread: thread) {
                    openRow(host, model, space, project, thread)
                }
            }
        }
    }

    /// Windows no thread claims that the list shows: with the eye on, the
    /// ones an agent runs in.
    private var windowsShown: Bool {
        (model.sidebar?.rows ?? []).contains { row in
            if case .window(_, _, _, let agent) = row { return !agentsOnly || agent }
            return false
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
        let anyWindow = windowsShown
        ForEach(tail) { row in
            switch row {
            case .archived(_, let open, let label):
                // The label carries the count.
                if !agentsOnly {
                    SidebarRow(icon: "archive", title: label, dim: true, chevron: open) {
                        model.sideClick("toggle-archived", flag: !open)
                    }
                }
            case .project(let id, let name, _, _, true):
                if !agentsOnly {
                    SidebarRow(icon: "folder", title: name, depth: 1, dim: true) {}
                        .contextMenu { AppMenuItems(model: model, kind: "archived-project", id: id) }
                }
            case .others:
                if anyWindow { ListSection(text: tr("otherwindows")) }
            case .window(let id, let title, let selected, let agent):
                if !agentsOnly || agent {
                    SidebarRow(icon: "app-window", title: title.isEmpty ? tr("window.n", id) : title, selected: selected) {
                        model.sideClick("window", id: String(id))
                        openHost()
                    }
                }
            default:
                EmptyView()
            }
        }
    }
}

/// A section's title in a list, small and grey as the desktop's.
private struct ListSection: View {
    var text: String
    var bold = false

    var body: some View {
        Text(text)
            .font(Font.subheadline.weight(bold ? .semibold : .regular))
            .foregroundColor(.secondary)
            .padding(.horizontal, 12)
            .padding(.top, 16)
            .padding(.bottom, 2)
    }
}

/// A thread's row: its cards, its name and its mark, the desktop's pill on
/// the one on show. A tap opens it (`SpaceList.openRow`); a long press is
/// its App's menu for it.
private struct ThreadItem: View {
    @ObservedObject var model: TerminalModel
    var thread: ThreadRow
    var subtitle: String? = nil
    var open: () -> Void

    var body: some View {
        SidebarRow(
            lead: AnyView(ThreadCards(core: model.core, icons: thread.icons ?? [], revision: model.iconsRevision)),
            mark: AnyView(ThreadMark(status: thread.markStatus, dot: thread.dot, live: thread.live, size: 9)),
            title: thread.name,
            subtitle: subtitle,
            dim: !thread.live,
            selected: thread.selected,
            action: open
        )
        .contextMenu { AppMenuItems(model: model, kind: "thread", id: thread.id) }
    }
}

// MARK: finding a thread

/// What the search looks for, apart from the list: only the field and the
/// finds read it, so typing redraws them and not the list round them (its
/// bars, their glass).
@Observable final class ThreadSearch {
    var query = ""
    /// The field takes the keyboard as it comes up next.
    @ObservationIgnored var focusNext = false
}

/// Every connected host's threads whose names hold the query, under the
/// host's name, each with the Space and the project it is in; a host with
/// none says nothing.
private struct Found: View {
    let search: ThreadSearch
    var hosts: [HostSession]
    var open: (UUID, TerminalModel, TreeSpace, TreeProject, ThreadRow) -> Void
    /// Moves on as any host's tree or connection changes, which this view
    /// does not observe one by one.
    @StateObject private var watch = TreesWatch()

    /// A thread found, and where it is.
    private struct Hit {
        let space: TreeSpace
        let project: TreeProject
        let thread: ThreadRow
    }

    private struct HostHits {
        let host: HostSession
        let hits: [Hit]
    }

    private func found(_ needle: String) -> [HostHits] {
        hosts.filter { $0.model.listReady }.compactMap { host -> HostHits? in
            let hits = (host.model.tree?.spaces ?? []).flatMap { space in
                space.projects.flatMap { project in
                    project.threads
                        .filter { needle.isEmpty || $0.name.localizedCaseInsensitiveContains(needle) }
                        .map { Hit(space: space, project: project, thread: $0) }
                }
            }
            return hits.isEmpty ? nil : HostHits(host: host, hits: hits)
        }
    }

    var body: some View {
        let _ = watch.watch(hosts.map(\.model))
        let groups = found(search.query.trimmingCharacters(in: .whitespaces))
        VStack(alignment: .leading, spacing: 2) {
            if groups.isEmpty {
                Text(tr("thread.find.none"))
                    .font(.subheadline)
                    .foregroundColor(.secondary)
                    .padding(12)
            }
            ForEach(groups, id: \.host.id) { group in
                ListSection(text: group.host.name, bold: true)
                ForEach(group.hits, id: \.thread.id) { hit in
                    ThreadItem(model: group.host.model, thread: hit.thread, subtitle: hit.space.name + " · " + hit.project.name) {
                        open(group.host.id, group.host.model, hit.space, hit.project, hit.thread)
                    }
                }
            }
        }
    }
}

/// The search's field as UIKit has it, which tells when a word is still
/// being put together -- pinyin before its characters are picked: the
/// query waits for the characters, as no thread is named in the letters.
private struct SearchField: UIViewRepresentable {
    let search: ThreadSearch
    let prompt: String

    func makeCoordinator() -> Coordinator { Coordinator(search: search) }

    func makeUIView(context: Context) -> UITextField {
        let field = UITextField()
        // What it held when the list last made it (a drawer shut and
        // opened again).
        field.text = search.query
        field.attributedPlaceholder = NSAttributedString(string: prompt, attributes: [.foregroundColor: UIColor.placeholderText])
        field.font = .preferredFont(forTextStyle: .body)
        field.adjustsFontForContentSizeCategory = true
        field.maximumContentSizeCategory = .extraExtraLarge
        field.textColor = .label
        field.autocapitalizationType = .none
        field.autocorrectionType = .no
        field.spellCheckingType = .no
        field.returnKeyType = .search
        field.delegate = context.coordinator
        field.addTarget(context.coordinator, action: #selector(Coordinator.edited(_:)), for: .editingChanged)
        field.setContentHuggingPriority(.defaultLow, for: .horizontal)
        field.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        DispatchQueue.main.async {
            if search.focusNext {
                search.focusNext = false
                field.becomeFirstResponder()
            }
        }
        return field
    }

    func updateUIView(_ field: UITextField, context: Context) {
        context.coordinator.search = search
    }

    final class Coordinator: NSObject, UITextFieldDelegate {
        var search: ThreadSearch

        init(search: ThreadSearch) { self.search = search }

        @objc func edited(_ field: UITextField) {
            guard field.markedTextRange == nil else { return }
            search.query = field.text ?? ""
        }

        func textFieldShouldReturn(_ field: UITextField) -> Bool {
            field.resignFirstResponder()
            return true
        }
    }
}

// MARK: reading the views, asking

/// How many parts of the app read a host's views now -- a list showing its
/// rows, a search, the repository groups -- so that one going does not stop
/// the views another still shows.
@MainActor private var listHolds: [ObjectIdentifier: Int] = [:]

/// Keeps `model`'s views read while this part of the app shows them, and
/// read as the core says they changed (`TerminalModel.setViewsListed`):
/// from the first part that reads them to the last.
@MainActor func keepViewsListed(_ model: TerminalModel) async {
    let key = ObjectIdentifier(model)
    listHolds[key, default: 0] += 1
    model.setViewsListed(true)
    while !Task.isCancelled {
        try? await Task.sleep(for: .seconds(1))
        // The terminal's own surface went with its screen, or the last
        // lend was refused: the list's is lent again.
        if !Task.isCancelled, model.generation == 0, model.isInFront { model.setViewsListed(true) }
    }
    let left = (listHolds[key] ?? 1) - 1
    if left > 0 {
        listHolds[key] = left
    } else {
        listHolds[key] = nil
        model.setViewsListed(false)
    }
}

/// What a list does for a host's session besides its rows: reads its
/// views while `reads`, and while `asks` puts what its App asks from a
/// row's menu (`HomeAsks`).
private struct ListChores: View {
    let model: TerminalModel
    var reads: Bool
    var asks: Bool

    var body: some View {
        ZStack {
            Color.clear
                .task(id: reads) {
                    if reads { await keepViewsListed(model) }
                }
            if asks {
                Color.clear.modifier(HomeAsks(model: model))
            }
        }
    }
}

/// What the App asks of the user from a row's menu in a list -- a new
/// name, a new project's folder, whether to delete a thread -- asked
/// there, not later on the terminal's screen; that screen asks for itself
/// while it shows the host.
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
            .onChange(of: model.nameAsks) { _, _ in
                // "+" asks at once, even while the core still asks from before.
                guard !model.isOnScreen else { return }
                renameText = ""
                renaming = "new-project"
            }
            .onChange(of: model.sidebar?.new_project_error) { _, error in
                // A folder the core refused: asked again, with why.
                if error != nil, !model.isOnScreen, model.sidebar?.editing.kind == "new-project", renaming == nil {
                    renaming = "new-project"
                }
            }
            .alert(
                renaming == "new-project" ? tr("project.new") : tr("rename"),
                isPresented: Binding(get: { renaming != nil }, set: { if !$0 { renaming = nil } })
            ) {
                // A new project is a folder on the host, ~/dir or /dir.
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
            } message: {
                if renaming == "new-project", let error = model.sidebar?.new_project_error {
                    Text(error)
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

/// What the lists are made of, watched: the hosts' trees and connections,
/// and only as they change -- a model publishes much else, its screen's
/// every change among it. Subscribed once for a set of hosts, not as the
/// view is drawn, so no change falls between two subscriptions.
private final class TreesWatch: ObservableObject {
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
                model.$pendingHostKey.map { $0 != nil }.removeDuplicates().dropFirst().map { _ in () }.eraseToAnyPublisher(),
            ]
        })
        .receive(on: RunLoop.main)
        .sink { [weak self] in self?.tick &+= 1 }
    }
}

// MARK: by repository

/// Every connected host's projects that are in a repository with a remote,
/// put together by repository: its name, then the threads of all its
/// copies. The group is the project, so which computer a thread is on is not
/// said -- unless that computer's connection is down -- and its branch only
/// when the group's threads are on more than one. A host not connected yet
/// has its projects here once it is.
private struct RepoGroups: View {
    /// A project of one host in a repository's group: that host's copy.
    struct Copy: Identifiable {
        let id: String
        let host: HostSession
        let space: TreeSpace
        let project: TreeProject
        let branch: String?
    }

    var hosts: [HostSession]
    /// A thread was started on the host with this id: its terminal comes up.
    var open: (UUID) -> Void
    var openRow: (UUID, TerminalModel, TreeSpace, TreeProject, ThreadRow) -> Void
    @ObservedObject private var settings = AppSettings.shared
    /// Moves on as any host's tree or connection changes, which this view
    /// does not observe one by one: their trees are read again.
    @StateObject private var watch = TreesWatch()

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

    private var groups: [Repo] {
        watch.watch(hosts.map(\.model))
        _ = watch.tick
        let live = hosts.filter { $0.model.listReady }
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
        project.threads.filter { shownThread($0, agentsOnly: settings.agentsOnly) }
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
                        ThreadItem(model: line.copy.host.model, thread: line.thread, subtitle: subtitle(line.copy, sayBranch: sayBranch)) {
                            openRow(line.copy.host.id, line.copy.host.model, line.copy.space, line.copy.project, line.thread)
                        }
                    }
                }
            }
        }
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
