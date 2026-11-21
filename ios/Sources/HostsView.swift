import SwiftUI

/// The hosts, each a row that opens its own page; connecting is that page's,
/// a row's menu's, or home's. The probe's local sshd is a debug-only row of
/// its own so the automated flows and a quick check need no setup.
struct HostsView: View {
    @EnvironmentObject var store: HostStore
    @EnvironmentObject var sessions: Sessions
    @ObservedObject private var lang = AppLanguage.shared
    @State private var adding = false
    @State private var search = ""
    @State private var pendingDelete: Host?
    /// A host others are reached through, asked to be deleted: it is not.
    @State private var jumpInUse: Host?
    @State private var connecting: ConnectTarget?
    /// A host about to be added for a machine of the tailnet.
    @State private var addingMachine: Host?
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

            // The tailnet ThinkTerm joined by itself, and its machines.
            TailnetSection(
                saved: { peer in
                    let names = peer.names(among: TailnetStore.shared.machines)
                    return store.hosts.first { names.contains($0.hostname.lowercased()) }
                },
                add: { addingMachine = $0 }
            )

            #if DEBUG
            Section(tr("dev")) {
                NavigationLink(value: probe) {
                    HostRowLayout(title: tr("thismac"), subtitle: "probe sshd on 127.0.0.1:2299") {
                        Image(systemName: "hammer").font(.system(size: 17, weight: .medium))
                    }
                }
            }
            #endif
        }
        .listStyle(.insetGrouped)
        .scrollContentBackground(.hidden)
        .background(Color.pageBackdrop.ignoresSafeArea())
        .navigationTitle(tr("hosts"))
        .navigationBarTitleDisplayMode(.large)
        .searchable(text: $search, placement: .navigationBarDrawer, prompt: tr("hosts.search"))
        .navigationDestination(for: Host.self) { host in
            #if DEBUG
            // The development row's stand-in host, told by its id: a real
            // host may well be called "probe". It has no page; it connects.
            if host.id == probe.id {
                TerminalScreen(model: sessions.model(for: nil, store: store))
            } else {
                page(host)
            }
            #else
            page(host)
            #endif
        }
        // A host's page opens its terminal by value, in this same stack.
        .navigationDestination(for: HostTerminal.self) { target in
            TerminalScreen(model: sessions.model(for: target.host, store: store))
        }
        // A separate destination type so the menu's Connect can push
        // without fighting the value-based destinations above.
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
            HostEditView(host: Host()) { host, secret, passphrase in keep(host, secret: secret, passphrase: passphrase) }
                .environmentObject(store)
        }
        .sheet(item: $addingMachine) { machine in
            HostEditView(host: machine) { host, secret, passphrase in keep(host, secret: secret, passphrase: passphrase) }
                .environmentObject(store)
        }
        .confirmationDialog(
            tr("host.delete.title"),
            isPresented: Binding(get: { pendingDelete != nil }, set: { if !$0 { pendingDelete = nil } }),
            titleVisibility: .visible,
            presenting: pendingDelete
        ) { host in
            Button(tr("host.delete.confirm", host.display), role: .destructive) { remove(host) }
            Button(tr("cancel"), role: .cancel) {}
        } message: { _ in
            Text(tr("host.delete.body"))
        }
        .alert(
            tr("jump.inuse.title"),
            isPresented: Binding(get: { jumpInUse != nil }, set: { if !$0 { jumpInUse = nil } }),
            presenting: jumpInUse
        ) { _ in
            Button(tr("ok"), role: .cancel) { jumpInUse = nil }
        } message: { host in
            Text(tr("jump.inuse.body", store.reachedThrough(host.id).map(\.display).joined(separator: ", ")))
        }
    }

    /// Delete, once asked: not a host others are reached through.
    private func askDelete(_ host: Host) {
        if store.reachedThrough(host.id).isEmpty { pendingDelete = host } else { jumpInUse = host }
    }

    /// A host's own page: its picture and session, its settings to edit.
    private func page(_ host: Host) -> some View {
        HostEditView(
            host: host,
            page: HostPage(
                sessions: sessions,
                // As the store has it now: the page may have saved edits.
                duplicate: { duplicate(store.hosts.first { $0.id == host.id } ?? host) },
                delete: { remove(host) }
            )
        ) { host, secret, passphrase in
            keep(host, secret: secret, passphrase: passphrase)
        }
    }

    @ViewBuilder
    private func row(_ host: Host) -> some View {
        NavigationLink(value: host) {
            HostRow(host: host, session: sessions.existing(host.id), line: store.line(for: host))
        }
        .swipeActions(edge: .trailing) {
            Button(role: .destructive) { askDelete(host) } label: { Label(tr("delete"), systemImage: "trash") }
            Button { connecting = ConnectTarget(host: host) } label: { Label(tr("connect"), systemImage: "bolt.horizontal") }
                .tint(.green)
        }
        .contextMenu {
            Button { connecting = ConnectTarget(host: host) } label: { Label(tr("connect"), systemImage: "bolt.horizontal") }
            if let session = sessions.existing(host.id), session.isConnected {
                // Disconnected, not closed: a session the list no longer
                // holds is one home connects again.
                Button { sessions.existing(host.id)?.disconnect() } label: { Label(tr("disconnect"), systemImage: "power") }
            }
            Button { duplicate(host) } label: { Label(tr("duplicate"), systemImage: "plus.square.on.square") }
            if host.knownHost != nil {
                Button { sessions.forgetHostKey(host.id, store: store) } label: { Label(tr("forgetkey"), systemImage: "xmark.shield") }
            }
            Divider()
            Button(role: .destructive) { askDelete(host) } label: { Label(tr("delete"), systemImage: "trash") }
        }
    }

    /// Keeps a host as edited or added, its secret in the Keychain; a
    /// session open on it takes the edit, as stored: another address, login
    /// or key is a connection made again.
    private func keep(_ host: Host, secret: String, passphrase: String?) {
        let before = store.hosts.first { $0.id == host.id }
        if !secret.isEmpty { Keychain.save(secret, account: host.id.uuidString) }
        if let passphrase { Keychain.save(passphrase, account: host.id.uuidString + ".passphrase") }
        store.upsert(host)
        guard let saved = store.hosts.first(where: { $0.id == host.id }) else { return }
        let credentialsChanged = !secret.isEmpty || passphrase != nil
        sessions.existing(host.id)?.applyHostEdit(saved, credentialsChanged: credentialsChanged, connectOnSave: false)
        sessions.jumpHostKept(before: before, saved: saved, credentialsChanged: credentialsChanged, store: store)
    }

    private func remove(_ host: Host) {
        sessions.close(host.id)
        store.remove(host)
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

/// The push target for the menu's Connect. Its own type so it does not
/// collide with the list's `navigationDestination(for: Host.self)`.
private struct ConnectTarget: Hashable {
    let host: Host
}

/// A host's terminal, pushed from its page.
struct HostTerminal: Hashable {
    let host: Host
}

/// One host: its picture, with a dot while its session is up or on its
/// way, its name and its address.
struct HostRow: View {
    let host: Host
    /// The host's open session, if any.
    var session: TerminalModel? = nil
    /// Its address and what it is reached through (`HostStore.line`).
    var line: String? = nil
    @ObservedObject private var lang = AppLanguage.shared

    var body: some View {
        HostRowLayout(title: host.title, subtitle: line ?? host.address, monospaced: true, session: session) {
            Image(systemName: host.iconSymbol).font(.system(size: 17, weight: .medium))
        }
    }
}

/// The square a host or a machine leads with: its picture on a quiet fill,
/// and the session's dot at the corner (`SessionDot`).
struct HostTile<Mark: View>: View {
    var size: CGFloat = 40
    var session: TerminalModel? = nil
    @ViewBuilder var mark: () -> Mark

    var body: some View {
        mark()
            .foregroundStyle(Color.primary.opacity(0.88))
            .frame(width: size, height: size)
            .background(
                RoundedRectangle(cornerRadius: size * 0.28, style: .continuous)
                    .fill(Color(.tertiarySystemFill))
            )
            .overlay(alignment: .bottomTrailing) {
                if let session {
                    SessionDot(model: session).offset(x: 3, y: 3)
                }
            }
    }
}

/// The shape of a row: a tile, a name, and an address under it.
struct HostRowLayout<Mark: View>: View {
    let title: String
    let subtitle: String
    /// The address in the terminal's kind of type.
    var monospaced = false
    var session: TerminalModel? = nil
    @ViewBuilder var mark: () -> Mark

    var body: some View {
        HStack(spacing: 12) {
            HostTile(session: session, mark: mark)
            VStack(alignment: .leading, spacing: 2) {
                Text(title).font(.body.weight(.semibold)).lineLimit(1)
                Text(subtitle)
                    .font(monospaced ? .footnote.monospaced() : .footnote)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            Spacer(minLength: 0)
        }
        .padding(.vertical, 3)
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

extension Host {
    /// The headline of a row: `display` would repeat the address below it
    /// when the host has no name.
    var title: String { name.isEmpty ? hostname : name }
}
