import SwiftUI

/// Where the app opens: the threads of one Space, of any of the hosts --
/// the Space on show picked at the list's head, as the desktop's Space
/// menu picks it, or swiped to -- the same list a terminal's sidebar is
/// (`SpaceList`). A thread tapped opens its host's terminal on it; a host
/// this phone has been in before is connected to as the screen comes up,
/// and one it has not waits for a tap.
struct ThreadsHome: View {
    @EnvironmentObject var store: HostStore
    @EnvironmentObject var sessions: Sessions
    @ObservedObject private var lang = AppLanguage.shared
    @State private var path = NavigationPath()
    @State private var adding = false

    /// The widest the list runs (an iPad's): wider, it keeps to the middle,
    /// as the grouped lists do there. Android's `READABLE_WIDTH`.
    static let readableWidth: CGFloat = 720

    /// What the list leads to.
    private enum Route: Hashable {
        /// A host's terminal, by the host's id (the probe's is
        /// `Sessions.probeId`); with `panel`, it comes with its sidebar out,
        /// a Space of it having been picked in another host's.
        case terminal(UUID, panel: Bool)
        case hosts
        case settings
    }

    var body: some View {
        NavigationStack(path: $path) {
            SpaceList(
                place: .home,
                onOpen: { id, _ in path.append(Route.terminal(id, panel: false)) },
                onSettings: { path.append(Route.settings) },
                onHosts: { path.append(Route.hosts) },
                onAddHost: { adding = true },
                live: path.isEmpty
            )
            .toolbar(.hidden, for: .navigationBar)
            .navigationDestination(for: Route.self) { route in
                switch route {
                case .terminal(let id, let panel):
                    TerminalScreen(model: model(id), panelOut: panel, onHost: { to, keepPanel in
                        switchTerminal(to: to, panel: keepPanel)
                    })
                case .hosts:
                    HostsView()
                case .settings:
                    SettingsView(showLog: nil)
                }
            }
        }
        // A terminal the hosts' pages opened goes to another host's from
        // its sidebar too: from Threads, as one opened here would be.
        .environment(\.openTerminal) { id, panel in
            var swap = Transaction()
            swap.disablesAnimations = true
            withTransaction(swap) {
                path = NavigationPath()
                path.append(Route.terminal(id, panel: panel))
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
                path.append(Route.terminal(seeded.id, panel: false))
            }
            connectKnownHosts()
        }
    }

    /// The session of the host with `id`, made when there is none.
    private func model(_ id: UUID) -> TerminalModel {
        sessions.model(for: store.hosts.first { $0.id == id }, store: store)
    }

    /// Another host's terminal in place of the one on show, from its
    /// sidebar (a thread or a Space of that host picked there): swapped,
    /// not pushed, so back from it is back here.
    private func switchTerminal(to id: UUID, panel: Bool) {
        var swap = Transaction()
        swap.disablesAnimations = true
        withTransaction(swap) {
            if !path.isEmpty { path.removeLast() }
            path.append(Route.terminal(id, panel: panel))
        }
    }

    /// Populate threads on arrival for known hosts that allow automatic connections.
    private func connectKnownHosts() {
        for entry in listedHosts(store) where sessions.existing(entry.id) == nil {
            if let host = entry.host, !host.autoConnect || host.lastConnected == nil { continue }
            model(entry.id).connect()
        }
    }
}

/// Show another host's terminal in place of the one on show, from Threads
/// (with its sidebar out when `panel`): for a terminal pushed somewhere
/// Threads' own routes do not reach, as the hosts' pages push theirs.
private struct OpenTerminalKey: EnvironmentKey {
    static let defaultValue: ((UUID, Bool) -> Void)? = nil
}

extension EnvironmentValues {
    var openTerminal: ((UUID, Bool) -> Void)? {
        get { self[OpenTerminalKey.self] }
        set { self[OpenTerminalKey.self] = newValue }
    }
}
