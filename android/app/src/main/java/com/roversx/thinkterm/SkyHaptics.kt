package com.roversx.thinkterm

import android.content.Context
import android.os.VibrationAttributes
import android.os.VibrationEffect
import android.os.VibrationEffect.Composition
import android.os.Vibrator
import android.os.VibratorManager
import kotlin.math.max
import kotlin.math.roundToInt

/// What the starry sky feels like (`LaunchSkyHost`, `ConnectionSky`): a
/// swell that grows with the stars as they rush past and falls away as the
/// app comes out from behind them. The launch's is the fullest, with a
/// faint stir while the stars draw near before it; a connection's is
/// lighter and shorter, a connection being made far more often than the app
/// is opened. Off with the setting, and felt as touch feedback is, so the
/// system's own setting for that holds too.
///
/// The phone's own rising and falling effects where it has them; else a
/// vibration whose strength follows the same curve, where the strength can
/// be set; else nothing -- a plain buzz is not this. The same shape as
/// iOS's `SkyHaptics`, every time and strength here.
object SkyHaptics {
    /// The launch: from the sky's start, the stir, then the rush at
    /// `LaunchSky.RUSH`. A first run's pages hold the rush back: `rush`
    /// plays it when they let it go.
    fun launch(context: Context) = play(context, rushAtMs = (LaunchSky.RUSH * 1000).roundToInt(), strength = 1f)

    /// The rush, from now: the welcome pages let go.
    fun rush(context: Context) = play(context, rushAtMs = 0, strength = 1f)

    /// A connection's sky rushing off to show the pane: lighter, and only
    /// for a sky that was there long enough to be seen.
    fun connected(context: Context, shownForSeconds: Float) {
        if (shownForSeconds >= SHOWN_LONG_ENOUGH) play(context, rushAtMs = 0, strength = CONNECT_STRENGTH)
    }

    // MARK: the shape

    /// The faint stir while the stars draw near, over the half second
    /// before the rush, from nothing to a breath.
    private const val STIR_MS = 500
    private const val STIR_PEAK = .1f

    /// The rush: how long the swell lasts, and how strong it is along the
    /// way (ms from the rush's start, strength). Stronger than iOS's: the
    /// same numbers feel fainter on an Android motor.
    private const val SWELL_MS = 420
    private val SWELL = listOf(0 to .15f, 150 to .3f, 280 to .55f, 380 to .85f, 420 to 1f)

    /// After the swell, how long it takes to fall away to nothing, so it
    /// ends rather than stops.
    private const val FALL_MS = 80

    /// A connection's rush, as a share of the launch's.
    private const val CONNECT_STRENGTH = .6f

    /// A sky up for less than this was a flicker, not a wait.
    private const val SHOWN_LONG_ENOUGH = .4f

    /// The steps the curve is drawn in, where it is drawn as a waveform.
    private const val STEP = 20

    // MARK: playing it

    private fun play(context: Context, rushAtMs: Int, strength: Float) {
        if (!AppSettings.get(context).skyHaptics) return
        val vibrator = context.getSystemService(VibratorManager::class.java)?.defaultVibrator ?: return
        if (!vibrator.hasVibrator()) return
        val effect = composed(vibrator, rushAtMs, strength)
            ?: if (vibrator.hasAmplitudeControl()) waveform(rushAtMs, strength) else return
        vibrator.vibrate(effect, VibrationAttributes.createForUsage(VibrationAttributes.USAGE_TOUCH))
    }

    /// The phone's slow rise for the swell, at the rush, then its quick
    /// fall straight after where it has one. One rise and no stir before
    /// it: a rise stops at its top, so two of them felt as a break.
    private fun composed(vibrator: Vibrator, rushAtMs: Int, strength: Float): VibrationEffect? {
        val rise = Composition.PRIMITIVE_SLOW_RISE
        if (!vibrator.areAllPrimitivesSupported(rise)) return null
        val peak = SWELL.last().second * strength
        val composition = VibrationEffect.startComposition()
            .addPrimitive(rise, peak, rushAtMs)
        if (vibrator.areAllPrimitivesSupported(Composition.PRIMITIVE_QUICK_FALL)) {
            composition.addPrimitive(Composition.PRIMITIVE_QUICK_FALL, peak, 0)
        }
        return composition.compose()
    }

    /// The same shape drawn as strengths over time.
    private fun waveform(rushAtMs: Int, strength: Float): VibrationEffect {
        val timings = mutableListOf<Long>()
        val amplitudes = mutableListOf<Int>()
        fun step(ms: Long, level: Float) {
            timings += ms
            amplitudes += (level.coerceIn(0f, 1f) * 255).roundToInt()
        }
        if (rushAtMs > 0) {
            val start = max(0, rushAtMs - STIR_MS)
            if (start > 0) step(start.toLong(), 0f)
            val steps = (rushAtMs - start) / STEP
            for (i in 1..steps) step(STEP.toLong(), STIR_PEAK * i / steps)
        }
        var at = 0
        while (at < SWELL_MS) {
            step(STEP.toLong(), swellAt(at) * strength)
            at += STEP
        }
        val peak = SWELL.last().second * strength
        val falls = FALL_MS / STEP
        for (i in 1..falls) step(STEP.toLong(), peak * (falls - i) / falls)
        return VibrationEffect.createWaveform(timings.toLongArray(), amplitudes.toIntArray(), -1)
    }

    /// The swell's strength `ms` into the rush, between its points.
    private fun swellAt(ms: Int): Float {
        val after = SWELL.indexOfFirst { it.first >= ms }
        if (after <= 0) return SWELL.first().second
        val (t0, v0) = SWELL[after - 1]
        val (t1, v1) = SWELL[after]
        return v0 + (v1 - v0) * (ms - t0) / (t1 - t0).toFloat()
    }
}
