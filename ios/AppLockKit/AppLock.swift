import LocalAuthentication
import SwiftUI
import UIKit

/// The app lock. With it on, the app opens behind a cover that the
/// phone's own check lifts: Face ID, or the passcode in its place. A fresh
/// start of the app is always locked; coming back to it is only once it
/// was away for `lockAfter` seconds or longer. The cover is over the app,
/// never instead of it: the connections and screens under it carry on.
///
/// The app's screens go inside `AppLockGate`; the settings call `turnOn`,
/// `turnOff` and `setLockAfter`. The words the cover shows are the app's,
/// handed in: this module knows nothing of the app. Main thread only, as
/// the app's own settings are; the checks, which wait, say so themselves.
public final class AppLock: ObservableObject {
    public static let shared = AppLock()

    /// How long the app may be away before coming back asks again, in
    /// seconds: 1, 5, 10 and 20 minutes, and an hour.
    public static let choices = [60, 300, 600, 1200, 3600]
    public static let defaultAfter = 300

    private static let onKey = "applock.on"
    private static let afterKey = "applock.after"

    @Published public private(set) var enabled: Bool
    @Published public private(set) var lockAfter: Int
    /// The cover is up.
    @Published public private(set) var locked: Bool
    /// Counts the covers put up, so each asks by itself once.
    @Published private(set) var lockCount = 0
    /// The system's check is up, or the app is still on its way back from
    /// it: the app going inactive under it is not the app leaving.
    @Published private(set) var asking = false

    /// The check is being asked now, so a second is not asked over it.
    private var inFlight = false
    /// When the app first left the active state, unlocked (to the app
    /// switcher, Control Center or the background), on the clock that runs
    /// on while the phone sleeps and that changing its time does not move.
    private var awayAt: UInt64?

    private init() {
        let defaults = UserDefaults.standard
        let on = defaults.bool(forKey: Self.onKey)
        let after = defaults.integer(forKey: Self.afterKey)
        enabled = on
        lockAfter = Self.choices.contains(after) ? after : Self.defaultAfter
        locked = false
        if on { lock() }
    }

    /// The phone has a passcode, which Face ID is set up under: without
    /// one there is nothing to check with.
    public var available: Bool {
        LAContext().canEvaluatePolicy(.deviceOwnerAuthentication, error: nil)
    }

    /// The phone says outright that it has no passcode. Not the same as
    /// `available` false, which other passing troubles say too: those keep
    /// the cover up.
    private var passcodeOff: Bool {
        var error: NSError?
        if LAContext().canEvaluatePolicy(.deviceOwnerAuthentication, error: &error) { return false }
        return error?.domain == LAError.errorDomain && error?.code == LAError.Code.passcodeNotSet.rawValue
    }

    /// Turns the lock on once the check passes here, so a lock that could
    /// not be lifted is never set. Says whether it is on.
    @MainActor @discardableResult
    public func turnOn(reason: String) async -> Bool {
        if enabled { return true }
        guard available else { return false }
        let (ok, _) = await ask(reason: reason)
        if ok {
            enabled = true
            awayAt = nil
            UserDefaults.standard.set(true, forKey: Self.onKey)
        }
        return ok
    }

    /// Turns the lock off once the check passes, as it went on: else
    /// anyone holding the app while it is open (back within the time set)
    /// could take the lock off for good. A phone with no passcode left has
    /// nothing to check with, and the lock lets anyone in already: off at
    /// once. Says whether it is off.
    @MainActor @discardableResult
    public func turnOff(reason: String) async -> Bool {
        if !enabled { return true }
        if !passcodeOff {
            let (ok, noPasscode) = await ask(reason: reason)
            guard ok || noPasscode || passcodeOff else { return false }
        }
        enabled = false
        locked = false
        awayAt = nil
        UserDefaults.standard.set(false, forKey: Self.onKey)
        return true
    }

    public func setLockAfter(_ seconds: Int) {
        guard Self.choices.contains(seconds) else { return }
        lockAfter = seconds
        UserDefaults.standard.set(seconds, forKey: Self.afterKey)
    }

    /// Out of the active state not for the lock's own check: the first
    /// such moment counts, through the background and back.
    func wentAway() {
        if enabled && !locked && awayAt == nil { awayAt = Self.now() }
    }

    /// In front again: back from the background, or from the check's own
    /// sheet over the app.
    func becameActive() {
        if !inFlight { asking = false }
        guard let at = awayAt else { return }
        awayAt = nil
        if enabled && !locked && Self.now() - at >= UInt64(lockAfter) * 1_000_000_000 { lock() }
    }

    private func lock() {
        locked = true
        lockCount += 1
    }

    /// Asks, and lifts the cover when the check passes. A phone whose
    /// passcode was turned off since has nothing to check with, and anyone
    /// may open it: the cover lifts with no question.
    @MainActor
    func unlock(reason: String) async {
        guard locked else { return }
        guard !passcodeOff else {
            locked = false
            return
        }
        let (ok, noPasscode) = await ask(reason: reason)
        if ok || noPasscode || passcodeOff { locked = false }
    }

    /// The system's check: Face ID, and the passcode for a face not set up
    /// or not recognised. Says whether it passed, and whether there was no
    /// passcode to check with.
    @MainActor
    private func ask(reason: String) async -> (ok: Bool, noPasscode: Bool) {
        guard !inFlight else { return (false, false) }
        inFlight = true
        asking = true
        var ok = false
        var noPasscode = false
        do {
            ok = try await LAContext().evaluatePolicy(.deviceOwnerAuthentication, localizedReason: reason)
        } catch let error as LAError {
            noPasscode = error.code == .passcodeNotSet
        } catch {}
        inFlight = false
        // Still inactive, the app is on its way back from the check's
        // sheet: `becameActive` clears it once there.
        if UIApplication.shared.applicationState == .active { asking = false }
        return (ok, noPasscode)
    }

    private static func now() -> UInt64 {
        clock_gettime_nsec_np(CLOCK_MONOTONIC)
    }
}

/// The words the cover shows, in the app's language.
public struct AppLockTexts {
    /// Over the cover: "ThinkTerm is locked".
    public var locked: String
    /// Its button.
    public var unlock: String
    /// Why the system's check is asked, under its title.
    public var reason: String

    public init(locked: String, unlock: String, reason: String) {
        self.locked = locked
        self.unlock = unlock
        self.reason = reason
    }
}

/// The app inside the lock. While locked, the cover is over it twice: in
/// its own place, and in a window of its own above any sheet or alert the
/// app has up or puts up later. In front, the check is asked by itself,
/// once per lock. With the lock on, the app leaving the screen is
/// covered as well, so the app switcher's picture of it shows the cover.
/// `mark` is the app's own picture over the words.
public struct AppLockGate<Mark: View>: ViewModifier {
    private let texts: AppLockTexts
    private let mark: Mark
    @ObservedObject private var lock = AppLock.shared
    @Environment(\.scenePhase) private var phase
    /// The last cover asked for by itself (`AppLock.lockCount`).
    @State private var asked = 0

    public init(texts: AppLockTexts, @ViewBuilder mark: () -> Mark) {
        self.texts = texts
        self.mark = mark()
    }

    public func body(content: Content) -> some View {
        content
            // Hidden from VoiceOver too while covered.
            .accessibilityHidden(lock.locked)
            .overlay {
                if lock.locked {
                    LockScreen(texts: texts, showsWords: true, mark: mark, onUnlock: unlock)
                }
            }
            .onAppear { update(phase) }
            .onChange(of: phase) { _, now in
                switch now {
                case .background: lock.wentAway()
                case .inactive: if !lock.asking { lock.wentAway() }
                case .active: lock.becameActive()
                @unknown default: break
                }
                update(now)
            }
            .onChange(of: lock.locked) { update(phase) }
            .onChange(of: lock.enabled) { update(phase) }
            .onChange(of: lock.asking) { update(phase) }
    }

    private func update(_ phase: ScenePhase) {
        // Out of the active state with the lock on, the app is covered:
        // under the app switcher, or in the background, whose picture of
        // it the switcher shows. Not under the lock's own check, which
        // makes the app inactive as well; in the background, always.
        let away = lock.enabled && (phase == .background || phase == .inactive && !lock.asking)
        if lock.locked || away {
            if lock.locked {
                // What is typed must not reach a terminal under the cover.
                UIApplication.shared.sendAction(#selector(UIResponder.resignFirstResponder), to: nil, from: nil, for: nil)
            }
            LockWindow.shared.show(LockScreen(texts: texts, showsWords: lock.locked, mark: mark, onUnlock: unlock))
        } else {
            LockWindow.shared.hide()
        }
        if lock.locked && phase == .active && asked != lock.lockCount {
            asked = lock.lockCount
            unlock()
        }
    }

    private func unlock() {
        let reason = texts.reason
        Task { await AppLock.shared.unlock(reason: reason) }
    }
}

/// The cover: the app's mark, the words, and the button that asks again;
/// over the app leaving the screen, the mark alone, laid out the same so
/// nothing moves when it turns into the lock. It takes every touch, so
/// nothing under it is reached.
struct LockScreen<Mark: View>: View {
    let texts: AppLockTexts
    let showsWords: Bool
    let mark: Mark
    let onUnlock: () -> Void

    var body: some View {
        ZStack {
            Color(uiColor: .systemBackground).ignoresSafeArea()
            VStack(spacing: 0) {
                mark
                Text(texts.locked)
                    .font(.headline)
                    .multilineTextAlignment(.center)
                    .padding(.top, 18)
                    .opacity(showsWords ? 1 : 0)
                Button(texts.unlock, action: onUnlock)
                    .buttonStyle(.borderedProminent)
                    .padding(.top, 24)
                    .opacity(showsWords ? 1 : 0)
                    .disabled(!showsWords)
            }
            .padding(.horizontal, 32)
        }
        .contentShape(Rectangle())
        .onTapGesture {}
    }
}

/// The window the cover is shown in: over the app's own and over every
/// sheet and alert in it, and the one the keys go to while it is up.
@MainActor
final class LockWindow {
    static let shared = LockWindow()

    private var window: UIWindow?
    private var host: UIHostingController<AnyView>?

    func show<V: View>(_ view: V) {
        let scenes = UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }
        guard let scene = scenes.first(where: { $0.activationState == .foregroundActive })
            ?? scenes.first(where: { $0.activationState == .foregroundInactive })
            ?? scenes.first
        else { return }
        // Kept for the scene it was made in: a scene the system took away
        // and gave back is a new one, and gets a window of its own.
        if let host, window?.windowScene === scene {
            host.rootView = AnyView(view)
            return
        }
        hide()
        let host = UIHostingController(rootView: AnyView(view))
        host.view.backgroundColor = .clear
        let window = UIWindow(windowScene: scene)
        window.windowLevel = .alert + 1
        window.rootViewController = host
        window.makeKeyAndVisible()
        self.window = window
        self.host = host
    }

    func hide() {
        guard let window else { return }
        window.isHidden = true
        let scene = window.windowScene
        self.window = nil
        host = nil
        // The app's own window takes the keys again.
        scene?.windows.first { $0.windowLevel == .normal && !$0.isHidden }?.makeKey()
    }
}
