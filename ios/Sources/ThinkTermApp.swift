import SwiftUI

@main
struct ThinkTermApp: App {
    @StateObject private var store = HostStore()
    @StateObject private var sessions = Sessions()

    var body: some Scene {
        WindowGroup {
            Group {
                if ProcessInfo.processInfo.arguments.contains(where: { $0.hasPrefix("--auto") || $0.hasSuffix("test") }) {
                    // The automated flows go straight to the probe's host.
                    NavigationStack { TerminalScreen(model: sessions.model(for: nil, store: store)) }
                } else {
                    RootTabs(store: store)
                }
            }
            .environmentObject(store)
            .environmentObject(sessions)
            .preferredColorScheme(.dark)
        }
    }

    /// `--seed-probe-host`: save the probe as an ordinary host whose key
    /// text sits in the Keychain, and open it. Debug builds only.
    static func seededHost(into store: HostStore) -> Host? {
        #if DEBUG
        guard ProcessInfo.processInfo.arguments.contains("--seed-probe-host"),
              let pem = try? String(contentsOfFile: "/tmp/ttp-ssh/userkey", encoding: .utf8) else { return nil }
        var host = store.hosts.first(where: { $0.name == "Probe (key)" }) ?? Host()
        host.name = "Probe (key)"
        host.hostname = "127.0.0.1"
        host.port = 2299
        host.user = probeUserName()
        host.auth = .key
        host.group = "Local"
        host.remoteCommand = "/tmp/ttp-ssh/thinkterm-remote cli --prefer-mux proxy"
        host.publicKey = SSHKey.publicLine(ofPrivate: pem, comment: host.user)
        Keychain.save(pem, account: host.id.uuidString)
        store.upsert(host)
        return host
        #else
        return nil
        #endif
    }
}

/// The two tabs. Its own view so the language switch relabels the tab bar
/// with the rest of the app.
private struct RootTabs: View {
    let store: HostStore
    @EnvironmentObject private var sessions: Sessions
    @Environment(\.scenePhase) private var scenePhase
    @State private var path = NavigationPath()
    @ObservedObject private var lang = AppLanguage.shared

    var body: some View {
        TabView {
            NavigationStack(path: $path) {
                HostsView()
                    .onAppear {
                        // A saved host for the probe, with its key in the
                        // Keychain, opened over the list: the same path a
                        // real host takes, back included.
                        if path.isEmpty, let seeded = ThinkTermApp.seededHost(into: store) {
                            path.append(seeded)
                        }
                    }
            }
            .tabItem { Label(tr("hosts"), systemImage: "server.rack") }
            NavigationStack {
                SettingsView(model: nil, showLog: nil)
            }
            .tabItem { Label(tr("settings"), systemImage: "gearshape") }
        }
        // Every open connection hears about the background, on screen or not.
        .onChange(of: scenePhase) { _, phase in
            switch phase {
            case .background: sessions.detachForBackground()
            case .active: sessions.reattachAfterBackground()
            default: break
            }
        }
    }
}
