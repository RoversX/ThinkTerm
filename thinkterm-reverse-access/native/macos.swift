import AppKit
import CoreGraphics
import LocalAuthentication

/// What the system's dialog says, already in the user's language.
struct Text: Decodable {
    let Cancel: String
    let Reason: String
}

struct Intent: Decodable {
    let Id: String
    let Source: String
    let Targets: String
    let Input: Bool
    let Seconds: UInt64
    let Text: Text

    static func read() throws -> Intent {
        var data = Data()
        while data.count < 8192 {
            guard let byte = try FileHandle.standardInput.read(upToCount: 1), !byte.isEmpty else {
                throw Failure.invalid
            }
            if byte[0] == 10 {
                guard let object = try JSONSerialization.jsonObject(with: data) as? [String: Any],
                      Set(object.keys) == Set(["Id", "Source", "Targets", "Input", "Seconds", "Text"]) else {
                    throw Failure.invalid
                }
                let intent = try JSONDecoder().decode(Intent.self, from: data)
                guard intent.Id.count == 32,
                      intent.Id.utf8.allSatisfy({ (48...57).contains($0) || (65...70).contains($0) || (97...102).contains($0) }),
                      intent.Seconds > 0, intent.Seconds <= 7200,
                      validText(intent.Source, limit: 256, multiline: false),
                      validText(intent.Targets, limit: 4096, multiline: true),
                      [intent.Text.Cancel, intent.Text.Reason].allSatisfy({
                          validText($0, limit: 512, multiline: false)
                      }) else {
                    throw Failure.invalid
                }
                return intent
            }
            data.append(byte)
        }
        throw Failure.invalid
    }

    private static func validText(_ value: String, limit: Int, multiline: Bool) -> Bool {
        !value.isEmpty && value.utf8.count <= limit && value.unicodeScalars.allSatisfy {
            !CharacterSet.controlCharacters.contains($0) || (multiline && $0.value == 10)
        }
    }
}

enum Failure: Error { case invalid }

func emit(_ kind: String, id: String? = nil) {
    let event: [String: Any] = ["kind": kind, "id": id as Any? ?? NSNull()]
    guard let data = try? JSONSerialization.data(withJSONObject: event),
          let line = String(data: data, encoding: .utf8) else { exit(2) }
    FileHandle.standardOutput.write(Data((line + "\n").utf8))
}

func consoleSessionAvailable() -> Bool {
    guard let session = CGSessionCopyCurrentDictionary() as? [String: Any],
          session[kCGSessionOnConsoleKey as String] as? Bool == true,
          session[kCGSessionLoginDoneKey as String] as? Bool == true,
          let uid = session[kCGSessionUserIDKey as String] as? UInt32,
          uid == getuid() else { return false }
    return true
}

final class NativeAuth: NSObject, NSApplicationDelegate {
    private let intent: Intent?
    private var context: LAContext?
    private var consumed = false
    private var deadline = ContinuousClock.now.advanced(by: .seconds(120))
    private var timer: Timer?
    private var observers: [NSObjectProtocol] = []

    init(intent: Intent?) { self.intent = intent }

    func applicationDidFinishLaunching(_ notification: Notification) {
        guard consoleSessionAvailable() else { finish(false); return }
        let workspace = NSWorkspace.shared.notificationCenter
        for name in [NSWorkspace.willSleepNotification, NSWorkspace.sessionDidResignActiveNotification] {
            observers.append(workspace.addObserver(forName: name, object: nil, queue: .main) {
                [weak self] _ in self?.finish(false)
            })
        }
        DistributedNotificationCenter.default().addObserver(
            self, selector: #selector(unavailable),
            name: NSNotification.Name("com.apple.screenIsLocked"), object: nil)
        FileHandle.standardInput.readabilityHandler = { [weak self] handle in
            _ = handle.availableData
            DispatchQueue.main.async { self?.finish(false) }
        }
        timer = Timer.scheduledTimer(withTimeInterval: 1, repeats: true) { [weak self] _ in
            guard let self else { return }
            if !consoleSessionAvailable() || (self.intent != nil && ContinuousClock.now >= self.deadline) {
                self.finish(false)
            } else if self.intent == nil {
                emit("alive")
            }
        }
        if let intent {
            authenticate(intent)
        } else {
            emit("ready")
        }
    }

    /// The system's dialog is the only window: its reason states all that is
    /// being allowed, in words the host chose from the frozen intent.
    private func authenticate(_ intent: Intent) {
        let context = LAContext()
        self.context = context
        context.touchIDAuthenticationAllowableReuseDuration = 0
        context.localizedCancelTitle = intent.Text.Cancel
        var error: NSError?
        guard context.canEvaluatePolicy(.deviceOwnerAuthentication, error: &error) else {
            // No dialog will say so. Ending without a result reads as
            // unavailable, where a refusal would pass in silence.
            exit(2)
        }
        NSApp.activate(ignoringOtherApps: true)
        // The system's device-owner policy can use Touch ID, the account
        // password, or a paired Apple Watch, according to the user's setup.
        context.evaluatePolicy(.deviceOwnerAuthentication,
            localizedReason: intent.Text.Reason) {
            [weak self] success, _ in
            DispatchQueue.main.async {
                guard let self else { return }
                self.finish(success && consoleSessionAvailable() && ContinuousClock.now < self.deadline)
            }
        }
    }

    @objc private func unavailable() { finish(false) }

    private func finish(_ approved: Bool) {
        guard !consumed else { return }
        consumed = true
        context?.invalidate()
        context = nil
        timer?.invalidate()
        FileHandle.standardInput.readabilityHandler = nil
        if let intent { emit(approved ? "approved" : "rejected", id: intent.Id) }
        else { emit("unavailable") }
        NSApp.terminate(nil)
    }
}

guard CommandLine.arguments.count == 2,
      isatty(STDIN_FILENO) == 0, isatty(STDOUT_FILENO) == 0 else { exit(2) }
let mode = CommandLine.arguments[1]
let intent: Intent?
do {
    if mode == "--authenticate" { intent = try Intent.read() }
    else if mode == "--monitor" { intent = nil }
    else { exit(2) }
} catch { exit(2) }
let app = NSApplication.shared
let delegate = NativeAuth(intent: intent)
app.delegate = delegate
app.setActivationPolicy(.accessory)
app.run()
