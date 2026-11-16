import SwiftUI

@main
struct ThinkTermApp: App {
    @StateObject private var store = HostStore()
    @StateObject private var sessions = Sessions()
    @Environment(\.scenePhase) private var scenePhase

    var body: some Scene {
        WindowGroup {
            Group {
                if TerminalModel.automated, !ProcessInfo.processInfo.arguments.contains("--auto-home") {
                    // The automated flows go straight to the probe's host.
                    NavigationStack { TerminalScreen(model: sessions.model(for: nil, store: store)) }
                } else {
                    // `--auto-home` (debug builds) is the list with the
                    // probe's key trusted as the flows trust it: a
                    // screenshot of it needs no tap.
                    ThreadsHome()
                }
            }
            // A fresh start opens on the starry sky; the automated flows do
            // not. A first run with no host kept has its pages on it. Inside
            // the objects below, so the pages' own screens have them too.
            .modifier(LaunchSky(
                play: !TerminalModel.automated,
                onboard: Self.replayOnboarding || !AppSettings.shared.onboarded && store.hosts.isEmpty
            ))
            .environmentObject(store)
            .environmentObject(sessions)
            // The tailnet ThinkTerm joins by itself is up before any host
            // of it is dialled.
            .onAppear {
                #if DEBUG
                if ProcessInfo.processInfo.arguments.contains("--tailnettest") {
                    return TailnetStore.shared.debugJoin()
                }
                #endif
                TailnetStore.shared.apply()
            }
        }
        // Every open connection hears about the background, on screen or
        // not, whichever way the app was opened.
        .onChange(of: scenePhase) { _, phase in
            switch phase {
            case .background: sessions.detachForBackground()
            case .active: sessions.reattachAfterBackground()
            default: break
            }
        }
    }

    /// `--replay-onboarding`: the first run's pages again, hosts or not.
    /// Debug builds only.
    static var replayOnboarding: Bool {
        #if DEBUG
        ProcessInfo.processInfo.arguments.contains("--replay-onboarding")
        #else
        false
        #endif
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
