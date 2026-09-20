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

    func close(_ id: UUID) {
        models.removeValue(forKey: id)?.shutdown()
    }

    var all: [TerminalModel] { Array(models.values) }

    /// The app left the screen or came back: every connection hears.
    func detachForBackground() { all.forEach { $0.detachForBackground() } }
    func reattachAfterBackground() { all.forEach { $0.reattachAfterBackground() } }
}

/// A green dot for a host whose session is open behind the list.
struct ConnectionDot: View {
    @ObservedObject var model: TerminalModel

    var body: some View {
        if model.isConnected {
            Circle().fill(Color.green).frame(width: 8, height: 8)
        }
    }
}
