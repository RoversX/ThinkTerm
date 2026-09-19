import SwiftUI

@main
struct ThinkTermApp: App {
    @StateObject private var store = HostStore()

    var body: some Scene {
        WindowGroup {
            NavigationStack {
                if ProcessInfo.processInfo.arguments.contains(where: { $0.hasPrefix("--auto") || $0.hasSuffix("test") }) {
                    // The automated flows go straight to the probe's host.
                    TerminalScreen(host: nil, store: store)
                } else {
                    HostsView()
                }
            }
            .environmentObject(store)
            .preferredColorScheme(.dark)
        }
    }
}
