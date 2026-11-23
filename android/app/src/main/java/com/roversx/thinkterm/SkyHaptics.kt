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
/// swell that grows with the stars as they rush past, and a soft landing as
/// the app comes out from behind them. The launch's is the fullest, with a
/// faint stir while the stars draw near before it; a connection's is
/// lighter and shorter, a connection being made far more often than the app
/// is opened. Off with the setting, and felt as touch feedback is, so the
/// system's own setting for that holds too.
///
/// The phone's own rising and landing effects where it has them; else a
/// vibration whose strength follows the same curve, where the strength can
/// be set; else nothing -- a plain buzz is not this. The same shape as
/// iOS's `SkyHaptics`, every time and strength here.
object SkyHaptics {
    /// The launch: from the sky's start, the stir, then the rush at
    /// `LaunchSky.RUSH`. A first run's pages hold the rush back: `rush`
    /// plays it when they let it go.
    fun launch(context: Context) = play(context, rushAtMs = (LaunchSky.RUSH * 1000).roundToInt(), strength = 1f)

    /// The rush and the landing, from now: the welcome pages let go.
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
    /// way (ms from the rush's start, strength).
    private const val SWELL_MS = 420
    private val SWELL = listOf(0 to .08f, 150 to .16f, 280 to .32f, 380 to .5f, 420 to .56f)

    /// The landing: a soft, low tap as the app settles.
    private const val LANDING_AT_MS = 500
    private const val LANDING_MS = 30L
    private const val LANDING = .5f

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

    /// The phone's slow rise for the stir and the swell, and its thud (or
    /// click) for the landing, placed by how long the phone says each lasts.
    private fun composed(vibrator: Vibrator, rushAtMs: Int, strength: Float): VibrationEffect? {
        val rise = Composition.PRIMITIVE_SLOW_RISE
        val land = listOf(Composition.PRIMITIVE_THUD, Composition.PRIMITIVE_CLICK)
            .firstOrNull { vibrator.areAllPrimitivesSupported(it) } ?: return null
        if (!vibrator.areAllPrimitivesSupported(rise)) return null
        val riseMs = vibrator.getPrimitiveDurations(rise)[0]
        val composition = VibrationEffect.startComposition()
        // Where the last piece ends: each piece's delay counts from there.
        var end = 0
        if (rushAtMs > 0) {
            val start = max(0, rushAtMs - riseMs)
            composition.addPrimitive(rise, STIR_PEAK, start)
            end = start + riseMs
        }
        val swell = max(end, rushAtMs)
        composition.addPrimitive(rise, SWELL.last().second * strength, swell - end)
        end = swell + riseMs
        val landing = max(end, rushAtMs + LANDING_AT_MS)
        composition.addPrimitive(land, LANDING * strength, landing - end)
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
        step((LANDING_AT_MS - SWELL_MS).toLong(), 0f)
        step(LANDING_MS, LANDING * strength)
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
