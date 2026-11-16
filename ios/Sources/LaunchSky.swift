import SwiftUI
import UIKit

/// The cold start's starry sky: stars come out on black and drift toward
/// the viewer, then rush past, and the app is there behind them. Once per
/// process, on the first screen of a fresh start; the app comes up and
/// connects behind it all the while. The launch screen is plain black
/// (Info.plist), so the sky starts where the system leaves off. On a first
/// run the welcome pages (`Onboarding`) sit on the sky, and the rush waits
/// for them. The same sky as Android's `LaunchSky`.
struct LaunchSky: ViewModifier {
    /// Played already in this process.
    static var played = false
    /// When a fresh start with nothing to ask rushes in, in seconds.
    static let rush: TimeInterval = 0.85
    /// From the rush to the app fully there.
    static let afterRush: TimeInterval = 0.6

    /// This screen opens on the sky: asked for, and not played yet. Both
    /// are settled when the screen is first made; the app's body is read
    /// again and again after, when `played` and the hosts have changed.
    @State private var active: Bool
    @State private var onboard: Bool
    @State private var started = false
    /// When the rush began, in seconds of the sky's own clock.
    @State private var rushAt: TimeInterval?
    @State private var revealed = false
    @State private var done = false
    @State private var start = Date()
    /// Where in the sky the pages are (`Travel`).
    @State private var travel = Travel()
    @ObservedObject private var again = WelcomeAgain.shared

    /// `onboard`: a first run's pages sit on the sky until they say so.
    init(play: Bool, onboard: Bool) {
        let active = play && !Self.played
        _active = State(initialValue: active)
        _onboard = State(initialValue: onboard && active)
    }

    func body(content: Content) -> some View {
        let hidden = active && !revealed
        content
            .opacity(hidden ? 0 : 1)
            .scaleEffect(hidden ? 1.06 : 1)
            .blur(radius: hidden ? 6 : 0)
            // The black sky has no status bar over it: its time and battery
            // follow the app's light or dark, and would be dark on black.
            // They come back as the sky rushes away.
            .statusBarHidden(hidden)
            .overlay {
                if active && !done {
                    ZStack {
                        StarSky(start: start, rushAt: rushAt, travel: travel)
                            .ignoresSafeArea()
                            // An app not yet in sight takes no touches.
                            .contentShape(Rectangle())
                            .onTapGesture {}
                        if onboard {
                            Onboarding(onPage: { page in
                                // The sky moves with the pages, forward and back.
                                let now = Date().timeIntervalSince(start)
                                travel = Travel(from: travel.camera(at: now), to: Double(page), at: now)
                            }) { rushIn() }
                                .opacity(rushAt == nil ? 1 : 0)
                                .animation(.easeOut(duration: 0.2), value: rushAt == nil)
                        }
                    }
                }
            }
            .onAppear {
                guard active, !started else { return }
                started = true
                Self.played = true
                start = Date()
                if !onboard {
                    DispatchQueue.main.asyncAfter(deadline: .now() + Self.rush) { rushIn() }
                }
            }
            .onChange(of: again.asks) { replay() }
    }

    /// The sky and the pages again, fading in over the app where it is. The
    /// keyboard goes first: what is typed must not reach a terminal the
    /// pages cover.
    private func replay() {
        UIApplication.shared.sendAction(#selector(UIResponder.resignFirstResponder), to: nil, from: nil, for: nil)
        started = true
        start = Date()
        travel = Travel()
        rushAt = nil
        onboard = true
        withAnimation(.easeOut(duration: 0.35)) {
            active = true
            done = false
            revealed = false
        }
    }

    /// The stars streak past, and the app comes out of the zoom and the
    /// blur as the sky goes.
    private func rushIn() {
        guard rushAt == nil else { return }
        if onboard { AppSettings.shared.onboarded = true }
        rushAt = Date().timeIntervalSince(start)
        withAnimation(.timingCurve(0.33, 0, 0.67, 1, duration: 0.47).delay(0.1)) { revealed = true }
        DispatchQueue.main.asyncAfter(deadline: .now() + Self.afterRush) { done = true }
    }
}

/// The settings' "Show Welcome", counted: each ask plays the sky and the
/// pages again.
final class WelcomeAgain: ObservableObject {
    static let shared = WelcomeAgain()
    @Published private(set) var asks = 0

    func show() { asks += 1 }
}

/// A move through the sky from one page's place to another's: a page
/// further on is deeper in, straight ahead.
private struct Travel {
    var from: Double = 0
    var to: Double = 0
    var at: TimeInterval = -10
    static let seconds: TimeInterval = 0.75

    func camera(at t: TimeInterval) -> Double {
        let x = min(max((t - at) / Self.seconds, 0), 1)
        let e = x < 0.5 ? 4 * x * x * x : 1 - pow(-2 * x + 2, 3) / 2
        return from + (to - from) * e
    }
}

/// A shooting star now and then while the pages sit on the sky: from
/// `x`, `y` across `dx`, `dy` in `life` seconds, from `at`.
private struct Meteor {
    var at: Double, x: Double, y: Double, dx: Double, dy: Double
    var length: Double, life: Double
}

private struct StarSky: View {
    let start: Date
    let rushAt: TimeInterval?
    let travel: Travel
    /// With the system's motion reduced, the sky only fades: no shooting
    /// stars, no trails.
    @Environment(\.accessibilityReduceMotion) private var still
    @State private var meteors: [Meteor] = []
    @State private var laidOut = CGSize.zero

    var body: some View {
        ZStack {
            // The stars themselves, made pixel by pixel (`SkyShader`).
            SkyMetal { size, scale in
                sky(at: Date().timeIntervalSince(start), size: size, scale: scale)
            }
            // Sixty a second is smooth for a shooting star, at half a
            // ProMotion screen's work.
            TimelineView(.animation(minimumInterval: 1.0 / 60, paused: still || rushAt != nil)) { timeline in
                Canvas { context, _ in
                    drawMeteors(&context, t: timeline.date.timeIntervalSince(start))
                }
            }
        }
        .onGeometryChange(for: CGSize.self) { $0.size } action: { size in
            guard size != laidOut, size.width > 0 else { return }
            laidOut = size
            meteors = Self.meteors(in: size)
        }
    }

    private func sky(at t: Double, size: CGSize, scale: CGFloat) -> SkyUniforms {
        func seg(_ a: Double, _ b: Double) -> Double { min(max((t - a) / (b - a), 0), 1) }
        func inOut(_ x: Double) -> Double { x < 0.5 ? 4 * x * x * x : 1 - pow(-2 * x + 2, 3) / 2 }
        let into = rushAt.map { seg($0 + 0.1, $0 + 0.57) } ?? 0
        // The sky comes a little closer, then wanders on slowly while pages
        // sit on it; at the rush it streaks past.
        let drift = 0.22 * inOut(seg(0.15, 1.4))
        let rush = rushAt.map { pow(seg($0, $0 + 0.45), 3) * 2.6 } ?? 0
        let fade = rushAt.map { 1 - seg($0 + 0.25, $0 + 0.55) } ?? 1
        let camera = travel.camera(at: t)
        let moving = camera - travel.camera(at: t - 0.03)
        return SkyUniforms(
            sizeX: Float(size.width * scale), sizeY: Float(size.height * scale), scale: Float(scale), time: Float(t),
            drift: Float(drift), camera: Float(camera), cameraPrev: Float(still ? camera : camera - moving * 2),
            rush: Float(rush), rushPrev: Float(still ? rush : rush * 0.9), fade: Float(fade),
            bg: Float(1 - inOut(into)), still: still ? 1 : 0
        )
    }

    /// One every three to seven seconds, the first after the sky has
    /// settled; enough for a long stay on the pages.
    private static func meteors(in size: CGSize) -> [Meteor] {
        var r = SystemRandomNumberGenerator()
        func next() -> Double { Double.random(in: 0..<1, using: &r) }
        var at = 2.2 + next() * 2
        return (0..<200).map { _ in
            let length = 70 + next() * 70
            let angle = (200 + next() * 32) * .pi / 180
            let travel = length * (1.5 + next() * 0.6)
            let meteor = Meteor(
                at: at, x: size.width * (0.2 + next() * 0.85), y: size.height * (-0.02 + next() * 0.4),
                dx: cos(angle) * travel, dy: -sin(angle) * travel,
                length: length, life: 0.55 + next() * 0.35
            )
            at += 3 + next() * 4
            return meteor
        }
    }

    private func drawMeteors(_ context: inout GraphicsContext, t: Double) {
        guard !still, rushAt == nil else { return }
        for m in meteors {
            let age = t - m.at
            if age < 0 || age > m.life { continue }
            let p = age / m.life
            let head = CGPoint(x: m.x + m.dx * p, y: m.y + m.dy * p)
            // The tail draws out behind the head, and the whole of it comes
            // and goes.
            let norm = hypot(m.dx, m.dy)
            let tailLength = m.length * min(max(p * 3, 0), 1)
            let tail = CGPoint(x: head.x - m.dx / norm * tailLength, y: head.y - m.dy / norm * tailLength)
            let a = sin(p * .pi) * 0.9
            var streak = Path()
            streak.move(to: tail)
            streak.addLine(to: head)
            context.stroke(
                streak,
                with: .linearGradient(Gradient(colors: [.white.opacity(0), .white.opacity(a)]), startPoint: tail, endPoint: head),
                style: StrokeStyle(lineWidth: 1.3, lineCap: .round)
            )
            context.fill(
                Path(ellipseIn: CGRect(x: head.x - 5, y: head.y - 5, width: 10, height: 10)),
                with: .radialGradient(
                    Gradient(colors: [Color(red: 200 / 255, green: 220 / 255, blue: 1).opacity(a * 0.45), .clear]),
                    center: head, startRadius: 0, endRadius: 5
                )
            )
            context.fill(Path(ellipseIn: CGRect(x: head.x - 1.2, y: head.y - 1.2, width: 2.4, height: 2.4)), with: .color(.white.opacity(a)))
        }
    }
}
