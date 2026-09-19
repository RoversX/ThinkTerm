import SwiftUI

/// The hosts, and the way in. The probe's local sshd is a row of its own
/// so the automated flows and a quick check need no setup.
struct HostsView: View {
    @EnvironmentObject var store: HostStore
    @State private var editing: Host?
    @State private var adding = false

    var body: some View {
        List {
            Section {
                ForEach(store.hosts) { host in
                    NavigationLink(value: host) {
                        VStack(alignment: .leading, spacing: 2) {
                            Text(host.display).font(.body)
                            Text("\(host.user)@\(host.hostname):\(host.port) · \(host.auth.label)")
                                .font(.caption).foregroundColor(.secondary)
                        }
                    }
                    .swipeActions(edge: .trailing) {
                        Button(role: .destructive) { store.remove(host) } label: { Label("Delete", systemImage: "trash") }
                        Button { editing = host } label: { Label("Edit", systemImage: "pencil") }
                    }
                    .contextMenu {
                        Button("Edit") { editing = host }
                        if host.knownHost != nil {
                            Button("Forget host key") { store.forgetHostKey(for: host.id) }
                        }
                        Button("Delete", role: .destructive) { store.remove(host) }
                    }
                }
            } header: {
                Text("Hosts")
            } footer: {
                if store.hosts.isEmpty {
                    Text("Add a host that has ThinkTerm installed. The app connects over ssh and attaches to the host's mux server.")
                }
            }
            Section("Development") {
                NavigationLink("This Mac (probe sshd on :2299)", value: Host(name: "probe", hostname: "127.0.0.1", port: 2299, user: NSUserName()))
            }
        }
        .navigationTitle("ThinkTerm")
        .navigationDestination(for: Host.self) { host in
            if host.name == "probe" {
                TerminalScreen(host: nil, store: store)
            } else {
                TerminalScreen(host: host, store: store)
            }
        }
        .toolbar {
            ToolbarItem(placement: .topBarTrailing) {
                Button { adding = true } label: { Image(systemName: "plus") }
            }
        }
        .sheet(isPresented: $adding) {
            HostEditView(host: Host()) { host, secret, passphrase in
                Keychain.save(secret, account: host.id.uuidString)
                if let passphrase { Keychain.save(passphrase, account: host.id.uuidString + ".passphrase") }
                store.upsert(host)
            }
        }
        .sheet(item: $editing) { host in
            HostEditView(host: host) { host, secret, passphrase in
                if !secret.isEmpty { Keychain.save(secret, account: host.id.uuidString) }
                if let passphrase { Keychain.save(passphrase, account: host.id.uuidString + ".passphrase") }
                store.upsert(host)
            }
        }
    }
}

struct HostEditView: View {
    @Environment(\.dismiss) private var dismiss
    @State var host: Host
    @State private var secret = ""
    @State private var passphrase = ""
    var save: (Host, String, String?) -> Void

    var body: some View {
        NavigationStack {
            Form {
                Section("Host") {
                    TextField("Name (optional)", text: $host.name)
                    TextField("Hostname or IP", text: $host.hostname)
                        .textInputAutocapitalization(.never).autocorrectionDisabled().keyboardType(.URL)
                    TextField("Port", value: $host.port, format: .number).keyboardType(.numberPad)
                    TextField("User", text: $host.user)
                        .textInputAutocapitalization(.never).autocorrectionDisabled()
                }
                Section("Authentication") {
                    Picker("Method", selection: $host.auth) {
                        ForEach(Host.AuthKind.allCases) { kind in Text(kind.label).tag(kind) }
                    }
                    .pickerStyle(.segmented)
                    if host.auth == .key {
                        TextEditor(text: $secret)
                            .font(.system(size: 11, design: .monospaced))
                            .frame(minHeight: 120)
                            .autocorrectionDisabled()
                            .textInputAutocapitalization(.never)
                        Text("Paste the private key (OpenSSH or PEM). Its public half must be in the host's authorized_keys.")
                            .font(.caption).foregroundColor(.secondary)
                        SecureField("Key passphrase (if any)", text: $passphrase)
                    } else {
                        SecureField("Password", text: $secret)
                    }
                }
                Section("Advanced") {
                    TextField("Remote command (blank = thinkterm cli --prefer-mux proxy)", text: $host.remoteCommand)
                        .textInputAutocapitalization(.never).autocorrectionDisabled()
                        .font(.caption)
                    if let known = host.knownHost {
                        Text("Host key: \(known)").font(.caption2).foregroundColor(.secondary)
                    }
                }
            }
            .navigationTitle(host.hostname.isEmpty ? "New host" : host.display)
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Save") {
                        save(host, secret, passphrase.isEmpty ? nil : passphrase)
                        dismiss()
                    }
                    .disabled(host.hostname.isEmpty || host.user.isEmpty)
                }
            }
        }
    }
}
