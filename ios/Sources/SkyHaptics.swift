import CoreHaptics

/// What the starry sky feels like (`LaunchSky`, the connection page's
/// sky): a swell that grows with the stars as they rush past and ends as
/// the app comes out from behind them. The launch's is the fullest, with a faint stir while the stars draw near before it; a
/// connection's is lighter and shorter, a connection being made far more
/// often than the app is opened. Off with the setting; on a phone with no
/// haptics, nothing.
///
/// Every time and strength is here, the shape the sky's own curves give
/// (`StarSky.sky`): the rush grows as the cube of its time over 0.45 s,
/// the app is there 0.57 s after it began.
enum SkyHaptics {
    /// The launch: from the sky's start, the stir, then the rush at
    /// `LaunchSky.rush`. A first run's pages hold the rush back: `rush`
    /// plays it when they let it go.
    static func launch() {
        play(stir(endingAt: LaunchSky.rush) + rushEvents(at: LaunchSky.rush, strength: 1))
    }

    /// The rush, from now: the welcome pages let go.
    static func rush() {
        play(rushEvents(at: 0, strength: 1))
    }

    /// A connection's sky rushing off to show the pane: lighter, and only
    /// for a sky that was there long enough to be seen.
    static func connected(shownFor seconds: TimeInterval) {
        guard seconds >= shownLongEnough else { return }
        play(rushEvents(at: 0, strength: connectStrength))
    }

    // MARK: the shape

    /// The faint stir while the stars draw near: from nothing to a breath,
    /// over the half second before the rush.
    private static let stirLength: TimeInterval = 0.5
    private static let stirPeak: Float = 0.1

    /// The rush: how long the swell lasts, and how strong and how sharp it
    /// is along the way (time from the rush's start, value).
    private static let swellLength: TimeInterval = 0.42
    private static let swellIntensity: [(TimeInterval, Float)] = [(0, 0.08), (0.15, 0.16), (0.28, 0.32), (0.38, 0.5), (0.42, 0.56)]
    private static let swellSharpness: [(TimeInterval, Float)] = [(0, 0.1), (0.42, 0.4)]

    /// A connection's rush, as a share of the launch's.
    private static let connectStrength: Float = 0.6
    /// A sky up for less than this was a flicker, not a wait.
    private static let shownLongEnough: TimeInterval = 0.4

    private static func stir(endingAt end: TimeInterval) -> [Piece] {
        let start = max(0, end - stirLength)
        return [.swell(
            at: start, length: end - start,
            intensity: [(0, 0), (end - start, stirPeak)],
            sharpness: [(0, 0.05), (end - start, 0.05)]
        )]
    }

    private static func rushEvents(at start: TimeInterval, strength: Float) -> [Piece] {
        [.swell(
            at: start, length: swellLength,
            intensity: swellIntensity.map { ($0.0, $0.1 * strength) },
            sharpness: swellSharpness
        )]
    }

    // MARK: playing it

    private enum Piece {
        /// A continuous vibration whose strength and sharpness follow the
        /// points given, in seconds from its own start.
        case swell(at: TimeInterval, length: TimeInterval, intensity: [(TimeInterval, Float)], sharpness: [(TimeInterval, Float)])
    }

    private static var engine: CHHapticEngine?

    private static func play(_ pieces: [Piece]) {
        guard AppSettings.shared.skyHaptics, CHHapticEngine.capabilitiesForHardware().supportsHaptics else { return }
        var events: [CHHapticEvent] = []
        var curves: [CHHapticParameterCurve] = []
        for piece in pieces {
            switch piece {
            case let .swell(at, length, intensity, sharpness):
                // The event at full strength, the curves scaling it.
                events.append(CHHapticEvent(
                    eventType: .hapticContinuous,
                    parameters: [
                        CHHapticEventParameter(parameterID: .hapticIntensity, value: 1),
                        CHHapticEventParameter(parameterID: .hapticSharpness, value: 0),
                    ],
                    relativeTime: at, duration: length
                ))
                curves.append(curve(.hapticIntensityControl, at: at, intensity))
                curves.append(curve(.hapticSharpnessControl, at: at, sharpness))
            }
        }
        do {
            let engine = try readyEngine()
            let player = try engine.makePlayer(with: CHHapticPattern(events: events, parameterCurves: curves))
            try player.start(atTime: CHHapticTimeImmediate)
        } catch {
            // Felt or not, the sky goes on.
            Self.engine = nil
        }
    }

    private static func curve(_ id: CHHapticDynamicParameter.ID, at start: TimeInterval, _ points: [(TimeInterval, Float)]) -> CHHapticParameterCurve {
        CHHapticParameterCurve(
            parameterID: id,
            controlPoints: points.map { CHHapticParameterCurve.ControlPoint(relativeTime: $0.0, value: $0.1) },
            relativeTime: start
        )
    }

    /// The engine, started; it stops itself once what it plays is over,
    /// and starts again on the next play.
    private static func readyEngine() throws -> CHHapticEngine {
        let engine = try Self.engine ?? CHHapticEngine()
        if Self.engine == nil {
            engine.playsHapticsOnly = true
            engine.isAutoShutdownEnabled = true
            // Reset by the system (the app sent away, say): started again
            // on the next play.
            engine.resetHandler = { DispatchQueue.main.async { Self.engine = nil } }
            engine.stoppedHandler = { _ in }
            Self.engine = engine
        }
        try engine.start()
        return engine
    }
}
