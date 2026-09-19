import Foundation
import CryptoKit

/// An ed25519 key pair the app makes for a host, written the way OpenSSH
/// writes its own (`PROTOCOL.key`): the private half in the unencrypted
/// openssh-key-v1 container, which russh reads, and the public half as
/// one `authorized_keys` line.
struct SSHKey {
    let privatePEM: String
    let publicLine: String

    static func generate(comment: String) -> SSHKey {
        let key = Curve25519.Signing.PrivateKey()
        let seed = Data(key.rawRepresentation)
        let pub = Data(key.publicKey.rawRepresentation)

        var publicBlob = Data()
        publicBlob.sshString("ssh-ed25519")
        publicBlob.sshString(pub)

        // The private section: two matching check ints, the key, and
        // padding up to the (unencrypted) block size of 8.
        let check = UInt32.random(in: 0...UInt32.max)
        var section = Data()
        section.sshUInt32(check)
        section.sshUInt32(check)
        section.sshString("ssh-ed25519")
        section.sshString(pub)
        section.sshString(seed + pub)
        section.sshString(comment)
        var pad: UInt8 = 1
        while section.count % 8 != 0 {
            section.append(pad)
            pad += 1
        }

        var blob = Data("openssh-key-v1".utf8)
        blob.append(0)
        blob.sshString("none")
        blob.sshString("none")
        blob.sshString("")
        blob.sshUInt32(1)
        blob.sshString(publicBlob)
        blob.sshString(section)

        let body = blob.base64EncodedString(options: [.lineLength76Characters, .endLineWithLineFeed])
            .replacingOccurrences(of: "\r", with: "")
        let pem = "-----BEGIN OPENSSH PRIVATE KEY-----\n" + body + "\n-----END OPENSSH PRIVATE KEY-----\n"
        let line = "ssh-ed25519 " + publicBlob.base64EncodedString() + (comment.isEmpty ? "" : " " + comment)
        return SSHKey(privatePEM: pem, publicLine: line)
    }

    /// The public line of an unencrypted OpenSSH ed25519 private key,
    /// for a key that was pasted or imported; nil for anything else.
    static func publicLine(ofPrivate pem: String, comment: String) -> String? {
        let lines = pem.split(whereSeparator: \.isNewline).map(String.init)
        guard lines.first?.contains("BEGIN OPENSSH PRIVATE KEY") == true else { return nil }
        let body = lines.dropFirst().filter { !$0.hasPrefix("-----") }.joined()
        guard let blob = Data(base64Encoded: body) else { return nil }
        var reader = SSHReader(blob)
        guard reader.take(15) == Data("openssh-key-v1".utf8) + [0],
              let cipher = reader.string(), cipher == Data("none".utf8),
              reader.string() != nil, reader.string() != nil,
              reader.uint32() == 1,
              let publicBlob = reader.string() else { return nil }
        var pub = SSHReader(publicBlob)
        guard pub.string() == Data("ssh-ed25519".utf8) else { return nil }
        return "ssh-ed25519 " + publicBlob.base64EncodedString() + (comment.isEmpty ? "" : " " + comment)
    }
}

private extension Data {
    mutating func sshUInt32(_ v: UInt32) {
        var be = v.bigEndian
        append(Data(bytes: &be, count: 4))
    }
    mutating func sshString(_ d: Data) {
        sshUInt32(UInt32(d.count))
        append(d)
    }
    mutating func sshString(_ s: String) {
        sshString(Data(s.utf8))
    }
}

private struct SSHReader {
    let data: Data
    var at = 0
    init(_ data: Data) { self.data = data }

    mutating func take(_ n: Int) -> Data? {
        guard at + n <= data.count else { return nil }
        defer { at += n }
        return data.subdata(in: at..<(at + n))
    }
    mutating func uint32() -> UInt32? {
        guard let d = take(4) else { return nil }
        return d.reduce(0) { ($0 << 8) | UInt32($1) }
    }
    mutating func string() -> Data? {
        guard let n = uint32() else { return nil }
        return take(Int(n))
    }
}
