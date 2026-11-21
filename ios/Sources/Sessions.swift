import Network
import SwiftUI

/// The open connections, one per host, outliving the terminal screen:
/// leaving the screen takes the picture away and nothing else, so a host
/// list visited in between, or another host opened, finds them where
/// they were. A host is closed from the list, or with the app.
final class Sessions: ObservableObject {
    @Published private(set) var models: [UUID: TerminalModel] = [:]

    /// The probe has no host row; it keeps a slot of its own.
    static let probeId = UUID(uuidString: "00000000-0000-0000-0000-0000000000BE")!

    func model(for host: Host?, store: HostStore) -> TerminalModel {
        let key = host?.id ?? Self.probeId
        if let model = models[key] { return model }
        let model = TerminalModel(host: host, store: store)
        models[key] = model
        return model
    }

    func existing(_ id: UUID) -> TerminalModel? { models[id] }

    /// Forgets a host's pinned key, in the store and in a session open on
    /// it, whose next connection would otherwise still hold the host to
    /// the old one: it asks for the key the host has now.
    func forgetHostKey(_ id: UUID, store: HostStore) {
        store.forgetHostKey(for: id)
        // Not an edit of the connection: the one up stays, and one down
        // waits for the user's retry rather than dialling into the key's
        // question.
        existing(id)?.forgetHostKey()
        // Those reached through it ask for its key on their next dial too.
        for behind in store.reachedThrough(id) { existing(behind.id)?.forgetJumpHostKey() }
    }

    /// A host kept as edited: the sessions reached through it go the new
    /// way, when the way to it changed.
    func jumpHostKept(before: Host?, saved: Host, credentialsChanged: Bool, store: HostStore) {
        if let before, !credentialsChanged, before.hostname == saved.hostname, before.port == saved.port,
           before.user == saved.user, before.auth == saved.auth, before.knownHost == saved.knownHost {
            return
        }
        for behind in store.reachedThrough(saved.id) { existing(behind.id)?.applyJumpHostEdit() }
    }

    func close(_ id: UUID) {
        models.removeValue(forKey: id)?.shutdown()
    }

    var all: [TerminalModel] { Array(models.values) }

    /// When the app last left the screen.
    private var leftAt: Date?
    /// Away this long, the built-in tailnet's node is made anew on the way
    /// back: what it held -- its way to the control server and its relays
    /// -- went stale while the app slept, and every connection through it
    /// would wait out a timeout before finding so.
    private static let tailnetStaleAfter: TimeInterval = 120

    /// The app left the screen or came back: every connection hears.
    func detachForBackground() {
        leftAt = Date()
        all.forEach { $0.detachForBackground() }
    }

    func reattachAfterBackground() {
        if let leftAt, Date().timeIntervalSince(leftAt) >= Self.tailnetStaleAfter {
            restartTailnet()
        }
        leftAt = nil
        all.forEach {
            $0.reattachAfterBackground()
            // The phone may have gone light or dark meanwhile.
            $0.appearanceMayHaveChanged()
        }
    }

    // MARK: the network

    private let pathMonitor = NWPathMonitor()
    /// The network in use, as the last word about it named it: its first
    /// interface (the one the system prefers), or none while there is none.
    private var network: String?
    private var networkSeen = false

    init() {
        pathMonitor.pathUpdateHandler = { [weak self] path in
            let network = path.status == .satisfied
                ? path.availableInterfaces.first.map { "\($0.type)/\($0.name)" }
                : nil
            DispatchQueue.main.async { self?.networkIs(network) }
        }
        pathMonitor.start(queue: DispatchQueue(label: "thinkterm.network"))
    }

    deinit { pathMonitor.cancel() }

    /// How long a new network is let settle before the node is made anew on
    /// it: one that comes and goes, or hands over and back, costs one
    /// restart -- and every tailnet session it carries -- not one each.
    private static let networkSettle: TimeInterval = 2
    private var settling: DispatchWorkItem?
    /// When the node was last made anew from here, and when the change
    /// `settling` waits on was heard of.
    private var restartedAt: Date?
    private var changedAt = Date.distantPast
    /// A restart this close before a change was heard of was made on the
    /// new network already: the return from a long absence, which hears of
    /// the network it came back to at the same moment.
    private static let restartCovers: TimeInterval = 1

    private func restartTailnet() {
        restartedAt = Date()
        tailnetRestart()
    }

    /// Another network, or one back after none: once it has settled, the
    /// tailnet's node is made anew on it, and every connection is looked at
    /// then, not when its keep-alives give up.
    private func networkIs(_ network: String?) {
        defer {
            self.network = network
            networkSeen = true
        }
        guard networkSeen, network != self.network else { return }
        // Another change before the last one settled starts the wait again;
        // with no network at all there is nothing to make anew on.
        settling?.cancel()
        settling = nil
        guard network != nil else { return }
        changedAt = Date()
        // Unless that was just done: another restart would only cut the
        // connections the first is redialing.
        let settled = DispatchWorkItem { [weak self] in
            guard let self else { return }
            let since = changedAt.addingTimeInterval(-Self.restartCovers)
            if restartedAt.map({ $0 < since }) ?? true {
                restartTailnet()
            }
            all.forEach { $0.core.checkConnection() }
        }
        settling = settled
        DispatchQueue.main.asyncAfter(deadline: .now() + Self.networkSettle, execute: settled)
    }
}

/// Where a host's session stands, for its dot in the list and its page.
enum SessionState {
    /// No session, or one that never got anywhere.
    case none
    /// Dialling, attaching, or redialing by itself.
    case connecting
    case connected
    /// Lost, and not redialing (auto-reconnect off): the way back is a tap.
    case lost

    init(_ model: TerminalModel, autoReconnect: Bool) {
        // A redial in its first quiet moments shows as the session did.
        if model.reconnecting && !model.redialQuiet {
            self = autoReconnect ? .connecting : .lost
        } else if model.connection.hasPrefix("pane ") {
            self = .connected
        } else if model.isConnected {
            self = .connecting
        } else {
            self = .none
        }
    }

    /// The dot a host's picture wears: up, or on its way.
    var dot: Color? {
        switch self {
        case .connected: return .green
        case .connecting: return .orange
        case .none, .lost: return nil
        }
    }
}

/// The dot for a host whose session is open behind the list, or on its way.
struct SessionDot: View {
    @ObservedObject var model: TerminalModel
    @ObservedObject private var settings = AppSettings.shared
    /// What the dot is cut out of: the row's own background.
    var ring = Color(.secondarySystemGroupedBackground)

    var body: some View {
        if let color = SessionState(model, autoReconnect: model.autoReconnect).dot {
            Circle()
                .fill(color)
                .frame(width: 12, height: 12)
                .overlay(Circle().stroke(ring, lineWidth: 2.5))
        }
    }
}
