import Foundation
import Security

/// A host the app connects to. The secret (key text or password) is not
/// here: it lives in the Keychain under the host's id.
struct Host: Codable, Identifiable, Hashable {
    enum AuthKind: String, Codable, CaseIterable, Identifiable {
        case key
        case password
        var id: String { rawValue }
        var label: String { self == .key ? tr("f.key") : tr("f.password") }
    }

    var id: UUID = UUID()
    var name: String = ""
    var hostname: String = ""
    var port: Int = 22
    var user: String = ""
    var auth: AuthKind = .key
    /// The host key fingerprint seen on the first connection.
    var knownHost: String?
    /// Set when the remote runs the probe's wrapper rather than an
    /// installed `thinkterm`; empty means the default proxy command.
    var remoteCommand: String = ""
    /// A label the list groups by; empty is no group.
    var group: String = ""
    /// The public half of a key the app generated or could read, so it
    /// can be shown and copied to the host's authorized_keys.
    var publicKey: String?
    var autoConnect = true
    var lastConnected: Date?
    /// The picture the host wears, by the desktop's name for it
    /// (`Host.icons`); empty is the server.
    var icon: String = ""
    /// The saved host this one is reached through (`ssh -J`), if any. One
    /// hop: that host has none of its own.
    var jumpHost: UUID?

    var display: String {
        name.isEmpty ? "\(user)@\(hostname):\(port)" : name
    }

    var address: String { "\(user)@\(hostname)" + (port == 22 ? "" : ":\(port)") }

    init() {}

    // Fields added later are absent from hosts saved before them.
    private enum Keys: String, CodingKey {
        case id, name, hostname, port, user, auth, knownHost, remoteCommand, group, publicKey, lastConnected, icon, autoConnect, jumpHost
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: Keys.self)
        id = try c.decode(UUID.self, forKey: .id)
        name = try c.decodeIfPresent(String.self, forKey: .name) ?? ""
        hostname = try c.decodeIfPresent(String.self, forKey: .hostname) ?? ""
        port = try c.decodeIfPresent(Int.self, forKey: .port) ?? 22
        user = try c.decodeIfPresent(String.self, forKey: .user) ?? ""
        auth = try c.decodeIfPresent(AuthKind.self, forKey: .auth) ?? .key
        knownHost = try c.decodeIfPresent(String.self, forKey: .knownHost)
        remoteCommand = try c.decodeIfPresent(String.self, forKey: .remoteCommand) ?? ""
        group = try c.decodeIfPresent(String.self, forKey: .group) ?? ""
        publicKey = try c.decodeIfPresent(String.self, forKey: .publicKey)
        autoConnect = try c.decodeIfPresent(Bool.self, forKey: .autoConnect) ?? true
        lastConnected = try c.decodeIfPresent(Date.self, forKey: .lastConnected)
        icon = try c.decodeIfPresent(String.self, forKey: .icon) ?? ""
        jumpHost = try c.decodeIfPresent(UUID.self, forKey: .jumpHost)
    }
}

extension Host {
    /// The desktop's host icons (thinkterm-core's `HostIcon`), in the order
    /// its picker offers them; a host with none chosen wears the first.
    static let icons = ["server", "cloud", "globe", "laptop", "mac-mini", "desktop", "chip", "container"]

    /// The system's own symbol for an icon: the phone draws a host as the
    /// rest of iOS does, under the desktop's name for it.
    static func iconSymbol(_ icon: String) -> String {
        switch icon {
        case "cloud": return "cloud"
        case "globe": return "globe"
        case "laptop": return "laptopcomputer"
        case "mac-mini": return "macmini"
        case "desktop": return "desktopcomputer"
        case "chip": return "cpu"
        case "container": return "shippingbox"
        default: return "server.rack"
        }
    }

    var iconSymbol: String { Self.iconSymbol(icon) }
}

extension Host {
    init(name: String, hostname: String, port: Int, user: String) {
        self.init()
        self.name = name
        self.hostname = hostname
        self.port = port
        self.user = user
    }
}

/// The hosts, in the app's defaults. Small, and edited rarely.
final class HostStore: ObservableObject {
    @Published private(set) var hosts: [Host] = []
    private let key = "hosts.v1"

    init() {
        if let data = UserDefaults.standard.data(forKey: key),
           let hosts = try? JSONDecoder().decode([Host].self, from: data) {
            self.hosts = hosts
        }
    }

    func upsert(_ host: Host) {
        var host = host
        if let i = hosts.firstIndex(where: { $0.id == host.id }) {
            // Another address is another machine as far as its key goes:
            // the pin stays only while the host is where it was pinned.
            if hosts[i].hostname != host.hostname || hosts[i].port != host.port {
                host.knownHost = nil
            }
            hosts[i] = host
        } else {
            hosts.append(host)
        }
        save()
    }

    func remove(_ host: Host) {
        hosts.removeAll { $0.id == host.id }
        Keychain.delete(account: host.id.uuidString)
        Keychain.delete(account: host.id.uuidString + ".passphrase")
        save()
    }

    func rememberHostKey(_ fingerprint: String, for id: UUID) {
        guard let i = hosts.firstIndex(where: { $0.id == id }) else { return }
        if hosts[i].knownHost != fingerprint {
            hosts[i].knownHost = fingerprint
            save()
        }
    }

    /// A connection reached the terminal: the list shows when.
    func touchConnected(_ id: UUID) {
        guard let i = hosts.firstIndex(where: { $0.id == id }) else { return }
        hosts[i].lastConnected = Date()
        save()
    }

    /// The groups in use, for the editor's suggestions.
    var groups: [String] {
        Array(Set(hosts.map(\.group).filter { !$0.isEmpty })).sorted()
    }

    /// The host `host` is reached through, if it has one still saved.
    func jump(of host: Host) -> Host? {
        guard let id = host.jumpHost else { return nil }
        return hosts.first { $0.id == id }
    }

    /// The hosts reached through the host `id`: while there are any, it is
    /// not deleted, nor given a jump host of its own.
    func reachedThrough(_ id: UUID) -> [Host] {
        hosts.filter { $0.jumpHost == id }
    }

    /// The hosts `host` may be reached through: any other saved one with no
    /// jump host of its own -- one hop, so no chain and no circle. None for
    /// a host others are reached through.
    func jumpCandidates(for host: Host) -> [Host] {
        guard reachedThrough(host.id).isEmpty else { return [] }
        return hosts.filter { $0.id != host.id && $0.jumpHost == nil }
    }

    /// A host's address in a row, and what it is reached through.
    func line(for host: Host) -> String {
        guard let through = jump(of: host) else { return host.address }
        return host.address + "  ·  " + tr("jump.via", through.title)
    }

    func forgetHostKey(for id: UUID) {
        guard let i = hosts.firstIndex(where: { $0.id == id }) else { return }
        hosts[i].knownHost = nil
        save()
    }

    private func save() {
        if let data = try? JSONEncoder().encode(hosts) {
            UserDefaults.standard.set(data, forKey: key)
        }
    }
}

/// The secrets, in the Keychain: one generic-password item per host id,
/// readable only while the device is unlocked.
enum Keychain {
    private static let service = "com.roversx.thinkterm.ssh"

    static func save(_ secret: String, account: String) {
        let data = Data(secret.utf8)
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
        ]
        let attrs: [String: Any] = [
            kSecValueData as String: data,
            kSecAttrAccessible as String: kSecAttrAccessibleWhenUnlockedThisDeviceOnly,
        ]
        var status = SecItemUpdate(query as CFDictionary, attrs as CFDictionary)
        if status == errSecItemNotFound {
            var add = query
            add.merge(attrs) { $1 }
            status = SecItemAdd(add as CFDictionary, nil)
        }
        if status != errSecSuccess {
            // -34018 is a missing entitlement: an unsigned simulator build.
            NSLog("keychain: saving %@ failed with %d", account, status)
        }
    }

    static func load(account: String) -> String? {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne,
        ]
        var out: AnyObject?
        guard SecItemCopyMatching(query as CFDictionary, &out) == errSecSuccess,
              let data = out as? Data else { return nil }
        return String(data: data, encoding: .utf8)
    }

    static func delete(account: String) {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
        ]
        SecItemDelete(query as CFDictionary)
    }
}
