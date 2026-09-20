import SwiftUI

/// The hosts, and the way in. The probe's local sshd is a debug-only row of
/// its own so the automated flows and a quick check need no setup.
struct HostsView: View {
    @EnvironmentObject var store: HostStore
    @EnvironmentObject var sessions: Sessions
    @ObservedObject private var lang = AppLanguage.shared
    @State private var editing: Host?
    @State private var adding = false
    @State private var search = ""
    @State private var pendingDelete: Host?
    @State private var connecting: ConnectTarget?
    #if DEBUG
    // Built once: a fresh Host would get a new id on every body pass, and
    // NavigationLink needs the value to stay equal across redraws.
    @State private var probe = Host(name: "probe", hostname: "127.0.0.1", port: 2299, user: probeUserName())
    #endif

    var body: some View {
        List {
            if store.hosts.isEmpty {
                Section {
                    HostsEmptyState { adding = true }
                }
                .listRowBackground(Color.clear)
                .listRowSeparator(.hidden)
            } else if matches.isEmpty {
                Section {
                    Text(tr("hosts.nomatch.q", search))
                        .font(.subheadline)
                        .foregroundStyle(.secondary)
                        .frame(maxWidth: .infinity, alignment: .center)
                        .padding(.vertical, 24)
                }
                .listRowBackground(Color.clear)
                .listRowSeparator(.hidden)
            } else if sections.count == 1, sections[0].title == nil {
                Section {
                    ForEach(sections[0].hosts) { row($0) }
                }
            } else {
                ForEach(sections) { group in
                    Section(group.title ?? tr("hosts.other")) {
                        ForEach(group.hosts) { row($0) }
                    }
                }
            }

            #if DEBUG
            Section(tr("dev")) {
                NavigationLink(value: probe) {
                    HostRowLayout(
                        symbol: "hammer",
                        tint: .gray,
                        title: tr("thismac"),
                        subtitle: "probe sshd on 127.0.0.1:2299"
                    )
                }
            }
            #endif
        }
        .listStyle(.insetGrouped)
        .scrollContentBackground(.hidden)
        .background(Color.black.ignoresSafeArea())
        .navigationTitle(tr("hosts"))
        .navigationBarTitleDisplayMode(.large)
        .searchable(text: $search, placement: .navigationBarDrawer, prompt: tr("hosts.search"))
        .navigationDestination(for: Host.self) { host in
            if host.name == "probe" {
                TerminalScreen(model: sessions.model(for: nil, store: store))
            } else {
                TerminalScreen(model: sessions.model(for: host, store: store))
            }
        }
        // A separate destination type so the context menu's Connect can push
        // without fighting the value-based destination above.
        .navigationDestination(item: $connecting) { target in
            TerminalScreen(model: sessions.model(for: target.host, store: store))
        }
        .toolbar {
            ToolbarItem(placement: .topBarTrailing) {
                Button { adding = true } label: { Image(systemName: "plus") }
                    .accessibilityLabel(tr("hosts.add"))
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
        .sheet(item: $editing) { host in
            HostEditView(host: host) { host, secret, passphrase in
                if !secret.isEmpty { Keychain.save(secret, account: host.id.uuidString) }
                if let passphrase { Keychain.save(passphrase, account: host.id.uuidString + ".passphrase") }
                store.upsert(host)
            }
            .environmentObject(store)
        }
        .confirmationDialog(
            tr("host.delete.title"),
            isPresented: Binding(get: { pendingDelete != nil }, set: { if !$0 { pendingDelete = nil } }),
            titleVisibility: .visible,
            presenting: pendingDelete
        ) { host in
            Button(tr("host.delete.confirm", host.display), role: .destructive) { sessions.close(host.id); store.remove(host) }
            Button(tr("cancel"), role: .cancel) {}
        } message: { _ in
            Text(tr("host.delete.body"))
        }
    }

    @ViewBuilder
    private func row(_ host: Host) -> some View {
        NavigationLink(value: host) {
            HostRow(host: host, session: sessions.existing(host.id))
        }
        .swipeActions(edge: .trailing) {
            Button(role: .destructive) { pendingDelete = host } label: { Label(tr("delete"), systemImage: "trash") }
            Button { editing = host } label: { Label(tr("edit"), systemImage: "pencil") }
                .tint(.indigo)
        }
        .contextMenu {
            Button { connecting = ConnectTarget(host: host) } label: { Label(tr("connect"), systemImage: "bolt.horizontal") }
            if let session = sessions.existing(host.id), session.isConnected {
                Button { sessions.close(host.id) } label: { Label(tr("disconnect"), systemImage: "power") }
            }
            Button { editing = host } label: { Label(tr("edit"), systemImage: "pencil") }
            Button { duplicate(host) } label: { Label(tr("duplicate"), systemImage: "plus.square.on.square") }
            if host.knownHost != nil {
                Button { store.forgetHostKey(for: host.id) } label: { Label(tr("forgetkey"), systemImage: "xmark.shield") }
            }
            Divider()
            Button(role: .destructive) { pendingDelete = host } label: { Label(tr("delete"), systemImage: "trash") }
        }
    }

    /// A copy under a new id, with the Keychain secret copied across so the
    /// duplicate connects without being re-keyed.
    private func duplicate(_ host: Host) {
        var copy = host
        copy.id = UUID()
        copy.name = (host.name.isEmpty ? host.display : host.name) + tr("host.copysuffix")
        copy.lastConnected = nil
        if let secret = Keychain.load(account: host.id.uuidString) {
            Keychain.save(secret, account: copy.id.uuidString)
        }
        if let passphrase = Keychain.load(account: host.id.uuidString + ".passphrase") {
            Keychain.save(passphrase, account: copy.id.uuidString + ".passphrase")
        }
        store.upsert(copy)
    }

    private var matches: [Host] {
        let query = search.trimmingCharacters(in: .whitespaces).lowercased()
        guard !query.isEmpty else { return store.hosts }
        return store.hosts.filter { host in
            [host.name, host.hostname, host.user, host.group].contains { $0.lowercased().contains(query) }
        }
    }

    /// One section per group once any host has one, ungrouped hosts last;
    /// a single title-less section when nobody uses groups.
    private var sections: [HostSection] {
        let hosts = matches
        guard store.hosts.contains(where: { !$0.group.isEmpty }) else {
            return [HostSection(id: "all", title: nil, hosts: hosts)]
        }
        var byGroup: [String: [Host]] = [:]
        for host in hosts { byGroup[host.group, default: []].append(host) }
        var out = byGroup.keys.filter { !$0.isEmpty }.sorted().map {
            HostSection(id: "g:" + $0, title: $0, hosts: byGroup[$0] ?? [])
        }
        if let ungrouped = byGroup[""], !ungrouped.isEmpty {
            out.append(HostSection(id: "other", title: nil, hosts: ungrouped))
        }
        return out
    }
}

private struct HostSection: Identifiable {
    let id: String
    let title: String?
    let hosts: [Host]
}

/// The push target for the context menu's Connect. Its own type so it does
/// not collide with the list's `navigationDestination(for: Host.self)`.
private struct ConnectTarget: Hashable {
    let host: Host
}

struct HostRow: View {
    let host: Host
    /// The host's open session, if any: a dot says so.
    var session: TerminalModel? = nil
    @ObservedObject private var lang = AppLanguage.shared

    var body: some View {
        HostRowLayout(
            symbol: "server.rack",
            tint: host.tint,
            title: host.title,
            subtitle: host.address
        ) {
            HStack(spacing: 6) {
                if let session { ConnectionDot(model: session) }
                Text(host.lastConnectedText)
                    .font(.caption)
                    .foregroundStyle(.tertiary)
                Text(host.auth == .key ? tr("badge.key") : tr("badge.password"))
                    .font(.caption2.weight(.semibold))
                    .foregroundStyle(.secondary)
                    .padding(.horizontal, 7)
                    .padding(.vertical, 2)
                    .background(Capsule().fill(Color.secondary.opacity(0.18)))
            }
            .padding(.top, 1)
        }
    }
}

/// The shared shape of a row: a tinted disc, a name, an address, and an
/// optional third line.
struct HostRowLayout<Detail: View>: View {
    let symbol: String
    let tint: Color
    let title: String
    let subtitle: String
    @ViewBuilder var detail: () -> Detail

    var body: some View {
        HStack(spacing: 12) {
            ZStack {
                Circle().fill(tint.gradient).frame(width: 40, height: 40)
                Image(systemName: symbol)
                    .font(.system(size: 16, weight: .semibold))
                    .foregroundStyle(.white)
            }
            VStack(alignment: .leading, spacing: 2) {
                Text(title).font(.headline).lineLimit(1)
                Text(subtitle)
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                detail()
            }
            Spacer(minLength: 0)
        }
        .padding(.vertical, 6)
    }
}

extension HostRowLayout where Detail == EmptyView {
    init(symbol: String, tint: Color, title: String, subtitle: String) {
        self.init(symbol: symbol, tint: tint, title: title, subtitle: subtitle) { EmptyView() }
    }
}

struct HostsEmptyState: View {
    var add: () -> Void
    @ObservedObject private var lang = AppLanguage.shared

    var body: some View {
        VStack(spacing: 10) {
            Image(systemName: "server.rack")
                .font(.system(size: 46, weight: .light))
                .foregroundStyle(.secondary)
                .padding(.bottom, 4)
            Text(tr("hosts.empty"))
                .font(.title3.weight(.semibold))
            Text(tr("hosts.empty.body"))
                .font(.subheadline)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .fixedSize(horizontal: false, vertical: true)
            Button(action: add) {
                Label(tr("hosts.add"), systemImage: "plus")
                    .font(.headline)
                    .frame(maxWidth: .infinity)
            }
            .buttonStyle(.borderedProminent)
            .controlSize(.large)
            .padding(.top, 14)
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 32)
        .frame(maxWidth: .infinity)
    }
}

private let hostRelativeFormatter: RelativeDateTimeFormatter = {
    let formatter = RelativeDateTimeFormatter()
    formatter.unitsStyle = .abbreviated
    return formatter
}()

extension Host {
    /// A stable colour per host. String.hashValue is seeded per process, so
    /// the bytes are hashed here instead to survive a relaunch.
    /// The headline of a row: `display` would repeat the address below it
    /// when the host has no name.
    var title: String { name.isEmpty ? hostname : name }

    var tint: Color {
        let palette: [Color] = [.blue, .indigo, .purple, .pink, .orange, .teal, .green, .cyan]
        var hash: UInt64 = 5381
        for byte in (name + hostname + user).utf8 { hash = hash &* 33 &+ UInt64(byte) }
        return palette[Int(hash % UInt64(palette.count))]
    }

    var lastConnectedText: String {
        guard let last = lastConnected else { return tr("hosts.never") }
        // "13m ago" follows the app's language, not the phone's.
        hostRelativeFormatter.locale = Locale(identifier: L10n.currentTag)
        return hostRelativeFormatter.localizedString(for: last, relativeTo: Date())
    }
}
