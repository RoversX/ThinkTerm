package com.roversx.thinkterm

import android.app.Activity
import android.graphics.RenderEffect
import androidx.activity.compose.BackHandler
import androidx.compose.ui.platform.LocalView
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import android.graphics.Shader
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.runtime.withFrameNanos
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.asComposeRenderEffect
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.ShaderBrush
import android.graphics.RuntimeShader
import androidx.compose.ui.unit.dp
import com.kyant.backdrop.backdrops.LayerBackdrop
import com.kyant.backdrop.backdrops.layerBackdrop
import com.kyant.backdrop.backdrops.rememberLayerBackdrop
import kotlin.math.PI
import kotlin.math.max
import kotlin.math.pow
import kotlin.math.sin
import kotlin.random.Random

/// The cold start's starry sky: stars come out on black and drift toward
/// the viewer, then rush past, and the app is there behind them. Once per
/// process, on the first screen of a fresh start; the app comes up and
/// connects behind it all the while. The launch screen is plain black
/// (themes.xml), so the sky starts where the system leaves off. On a first
/// run the welcome pages (`Onboarding`) sit on the sky, and the rush waits
/// for them. The same sky as iOS's `LaunchSky`.
object LaunchSky {
    /// Played already in this process: a screen made again, or the app
    /// brought back while its process lived, starts without it.
    var played = false

    /// When a fresh start with nothing to ask rushes in, in seconds.
    const val RUSH = .85f

    /// From the rush to the app fully there.
    const val AFTER_RUSH = .6f

    /// The settings' "Show Welcome", counted: each ask plays the sky and
    /// the pages again over the app where it is.
    val again = mutableIntStateOf(0)

    /// The last ask played. A screen made again later in the process (a
    /// system setting changed, say) finds it answered and plays nothing.
    var answered = 0

    /// The sky is over the screen: the system's bars take light icons on
    /// it, whatever the screen under it is (`SystemBars`).
    var covering by mutableStateOf(false)

    /// One of the app's own pages is up over the sky (the pages' host
    /// editor, the tailnet's settings): the bars follow it, not the sky.
    var paged by mutableStateOf(false)

    fun showAgain() {
        again.intValue++
    }
}

/// A shooting star now and then while the pages sit on the sky: from
/// `x`, `y` across `dx`, `dy` in `life` seconds, from `at`.
private class Meteor(
    val at: Float, val x: Float, val y: Float, val dx: Float, val dy: Float,
    val length: Float, val life: Float,
)

private fun clamp01(x: Float) = x.coerceIn(0f, 1f)
private fun seg(t: Float, a: Float, b: Float) = clamp01((t - a) / (b - a))
private fun inOut(t: Float) = if (t < .5f) 4 * t * t * t else 1 - (-2 * t + 2).pow(3) / 2
private fun out(t: Float) = 1 - (1 - t).pow(3)
private fun lerp(a: Float, b: Float, t: Float) = a + (b - a) * t

/// How long moving to another page's place in the sky takes, in seconds.
private const val TRAVEL = .75f

/// How far the app has come out from behind the sky, 0 to 1.
private fun reveal(t: Float, rushAt: Float?) = if (rushAt == null) 0f else seg(t, rushAt + .1f, rushAt + .57f)

/// The app under the sky: unseen, then coming out of a slight zoom and
/// blur as the sky goes. With `play` false, the app alone. With `onboard`,
/// the `pages` sit on the sky until they call `enter`, and the rush waits
/// for it; `travel` to a page moves through the sky with it, deeper for a
/// later one. Asked for again (`LaunchSky.again`), the sky and the pages
/// fade in over the app and play once more.
@Composable
fun LaunchSkyHost(
    play: Boolean,
    onboard: Boolean,
    pages: @Composable (enter: () -> Unit, travel: (Int) -> Unit) -> Unit,
    content: @Composable () -> Unit,
) {
    val again = LaunchSky.again.intValue
    // An ask not played yet; taken as it is read, so it plays once.
    val replay = remember(again) { (again > LaunchSky.answered).also { LaunchSky.answered = again } }
    val withPages = onboard || replay
    var t by remember(again) { mutableFloatStateOf(0f) }
    var rushAt by remember(again) { mutableStateOf(if (withPages) null else LaunchSky.RUSH) }
    var running by remember(again) { mutableStateOf(play || replay) }
    LaunchedEffect(running) { LaunchSky.covering = running }
    // The pages' glass shows the sky under it.
    val sky = rememberLayerBackdrop()
    // Where in the sky the pages are, moving from one page's place to the
    // next's over `TRAVEL` seconds of the sky's clock.
    var travelFrom by remember(again) { mutableFloatStateOf(0f) }
    var travelTo by remember(again) { mutableFloatStateOf(0f) }
    var travelAt by remember(again) { mutableFloatStateOf(-10f) }
    fun camera(at: Float) = lerp(travelFrom, travelTo, inOut(seg(at, travelAt, travelAt + TRAVEL)))
    // A start comes out of the launch screen's black; a replay fades in
    // over the app.
    val fadeIn = if (replay) out(seg(t, 0f, .35f)) else 1f
    val view = LocalView.current
    val context = LocalContext.current
    // The welcome pages let the rush go, felt as it goes.
    fun rushNow() {
        if (rushAt == null) {
            rushAt = t
            SkyHaptics.rush(context)
        }
    }
    if (play || replay) {
        LaunchedEffect(again) {
            LaunchSky.played = true
            // Played again over the app, the keyboard goes with it: what is
            // typed must not reach a terminal the pages cover.
            if (replay) {
                view.rootView.findFocus()?.clearFocus()
                (view.context as? Activity)?.window?.let { window ->
                    WindowCompat.getInsetsController(window, view).hide(WindowInsetsCompat.Type.ime())
                }
            }
            val start = withFrameNanos { it }
            // Felt from the start: the stir as the stars draw near, then
            // the rush with them.
            if (!withPages) SkyHaptics.launch(context)
            while (true) {
                withFrameNanos { now -> t = (now - start) / 1e9f }
                val at = rushAt
                if (at != null && t >= at + LaunchSky.AFTER_RUSH) break
            }
            running = false
        }
    }
    Box(Modifier.fillMaxSize()) {
        Box(
            Modifier.fillMaxSize().graphicsLayer {
                if (!running) return@graphicsLayer
                val into = reveal(t, rushAt)
                alpha = 1 - (1 - inOut(into)) * fadeIn
                val away = (1 - out(into)) * fadeIn
                val scale = 1 + .06f * away
                scaleX = scale
                scaleY = scale
                val blur = away * 6.dp.toPx()
                renderEffect = if (blur >= .5f) {
                    RenderEffect.createBlurEffect(blur, blur, Shader.TileMode.CLAMP).asComposeRenderEffect()
                } else {
                    null
                }
            },
        ) { content() }
        if (running) {
            Sky(t, rushAt, sky, camera(t), camera(t) - camera(t - .03f), fadeIn)
            if (withPages) {
                // Back on the first page of a replay goes into the app, as
                // the button does; on a later page the pages go back.
                BackHandler(enabled = replay && rushAt == null) { rushNow() }
                val at = rushAt
                Box(Modifier.fillMaxSize().graphicsLayer { alpha = if (at == null) fadeIn else 1 - seg(t, at, at + .2f) }) {
                    CompositionLocalProvider(LocalGlass provides sky) {
                        pages(
                            { rushNow() },
                            { page ->
                                if (page.toFloat() != travelTo) {
                                    travelFrom = camera(t)
                                    travelTo = page.toFloat()
                                    travelAt = t
                                }
                            },
                        )
                    }
                }
            }
        }
    }
}

/// The sky on its own, under the connection page while a connection is on
/// its way (`TerminalOverlays`): it fades in over the terminal, the stars
/// come out and drift, and once `rushing` they streak past as its black
/// clears to the pane under it, as the app comes out from behind the
/// launch's. The iOS app plays the same.
@Composable
fun ConnectionSky(rushing: Boolean) {
    var t by remember { mutableFloatStateOf(0f) }
    var rushAt by remember { mutableStateOf<Float?>(null) }
    val context = LocalContext.current
    LaunchedEffect(rushing) {
        if (rushing && rushAt == null) {
            rushAt = t
            SkyHaptics.connected(context, shownForSeconds = t)
        }
    }
    LaunchedEffect(Unit) {
        val start = withFrameNanos { it }
        while (true) withFrameNanos { now -> t = (now - start) / 1e9f }
    }
    val sky = rememberLayerBackdrop()
    // Rushing off, it lets the pane under it take touches at once.
    Sky(t, rushAt, sky, 0f, 0f, out(seg(t, 0f, .35f)), takesTouches = !rushing)
}

/// The sky itself, over everything, taking the touches meant for an app
/// not yet in sight: the stars from `SKY_SHADER`, the shooting stars over
/// them.
@Composable
private fun Sky(
    t: Float,
    rushAt: Float?,
    glass: LayerBackdrop,
    camera: Float,
    moving: Float,
    fadeIn: Float,
    takesTouches: Boolean = true,
) {
    val density = LocalDensity.current.density
    // Compiled on the phone: a sky the GPU will not take is plain black,
    // never a start that fails.
    val shader = remember {
        runCatching { RuntimeShader(SKY_SHADER) }
            .onFailure { android.util.Log.w("thinkterm", "sky shader: $it") }
            .getOrNull()
    }
    val brush = remember(shader) { shader?.let { ShaderBrush(it) } }
    val meteors = remember { mutableListOf<Meteor>() }
    // With the system's animations off, the sky only fades: no shooting
    // stars, no trails.
    val context = LocalContext.current
    val still = remember {
        android.provider.Settings.Global.getFloat(context.contentResolver, android.provider.Settings.Global.ANIMATOR_DURATION_SCALE, 1f) == 0f
    }
    Canvas(
        Modifier.fillMaxSize().layerBackdrop(glass).then(
            if (takesTouches) {
                Modifier.pointerInput(Unit) {
                    awaitPointerEventScope {
                        while (true) awaitPointerEvent().changes.forEach { it.consume() }
                    }
                }
            } else {
                Modifier
            },
        ),
    ) {
        val w = size.width
        val h = size.height
        if (meteors.isEmpty()) {
            // One every three to seven seconds, the first after the sky
            // has settled; enough for a long stay on the pages.
            val r = Random(7)
            var at = 2.2f + r.nextFloat() * 2f
            repeat(200) {
                val length = (70 + r.nextFloat() * 70) * density
                val angle = Math.toRadians(200.0 + r.nextFloat() * 32).toFloat()
                val travel = length * (1.5f + r.nextFloat() * .6f)
                meteors += Meteor(
                    at = at,
                    x = w * (.2f + r.nextFloat() * .85f),
                    y = h * (-.02f + r.nextFloat() * .4f),
                    dx = kotlin.math.cos(angle) * travel,
                    dy = -kotlin.math.sin(angle) * travel,
                    length = length,
                    life = .55f + r.nextFloat() * .35f,
                )
                at += 3f + r.nextFloat() * 4f
            }
        }
        // The sky comes a little closer, then wanders on slowly while
        // pages sit on it; at the rush it streaks past.
        val drift = .22f * inOut(seg(t, .15f, 1.4f))
        val rush = if (rushAt == null) 0f else seg(t, rushAt, rushAt + .45f).pow(3) * 2.6f
        val fade = if (rushAt == null) 1f else 1 - seg(t, rushAt + .25f, rushAt + .55f)
        if (shader != null && brush != null) {
            shader.setFloatUniform("uSize", w, h)
            shader.setFloatUniform("uScale", density)
            shader.setFloatUniform("uTime", t)
            shader.setFloatUniform("uDrift", drift)
            shader.setFloatUniform("uCamera", camera)
            shader.setFloatUniform("uCameraPrev", if (still) camera else camera - moving * 2)
            shader.setFloatUniform("uRush", rush)
            shader.setFloatUniform("uRushPrev", if (still) rush else rush * .9f)
            shader.setFloatUniform("uFade", fade * fadeIn)
            shader.setFloatUniform("uBg", (1 - inOut(reveal(t, rushAt))) * fadeIn)
            shader.setFloatUniform("uStill", if (still) 1f else 0f)
            drawRect(brush)
        } else {
            drawRect(Color.Black.copy(alpha = (1 - inOut(reveal(t, rushAt))) * fadeIn))
        }
        if (!still && rushAt == null) {
            for (m in meteors) {
                val age = t - m.at
                if (age < 0 || age > m.life) continue
                val p = age / m.life
                val hx = m.x + m.dx * p
                val hy = m.y + m.dy * p
                // The tail draws out behind the head, and the whole of it
                // comes and goes.
                val norm = kotlin.math.hypot(m.dx, m.dy)
                val tail = m.length * clamp01(p * 3)
                val tx = hx - m.dx / norm * tail
                val ty = hy - m.dy / norm * tail
                val a = sin(p * PI.toFloat()) * .9f
                drawLine(
                    Brush.linearGradient(listOf(Color.White.copy(alpha = 0f), Color.White.copy(alpha = a)), Offset(tx, ty), Offset(hx, hy)),
                    Offset(tx, ty),
                    Offset(hx, hy),
                    strokeWidth = 1.3f * density,
                    cap = StrokeCap.Round,
                )
                drawCircle(
                    Brush.radialGradient(listOf(Color(200, 220, 255).copy(alpha = a * .45f), Color.Transparent), Offset(hx, hy), 5f * density),
                    5f * density,
                    Offset(hx, hy),
                )
                drawCircle(Color.White.copy(alpha = a), 1.2f * density, Offset(hx, hy))
            }
        }
    }
}
