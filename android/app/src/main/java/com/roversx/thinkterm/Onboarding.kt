package com.roversx.thinkterm

import android.content.Intent
import android.net.Uri
import androidx.activity.compose.BackHandler
import androidx.compose.animation.AnimatedContent
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.core.tween
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.slideInHorizontally
import androidx.compose.animation.slideInVertically
import androidx.compose.animation.slideOutHorizontally
import androidx.compose.animation.slideOutVertically
import androidx.compose.animation.togetherWith
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.ColorFilter
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalView
import androidx.compose.foundation.gestures.detectHorizontalDragGestures
import androidx.compose.ui.input.pointer.pointerInput
import android.view.HapticFeedbackConstants
import com.kyant.backdrop.drawBackdrop
import com.kyant.backdrop.effects.blur
import com.kyant.backdrop.effects.lens
import com.kyant.backdrop.effects.vibrancy
import com.kyant.backdrop.highlight.Highlight
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp

/// The project's home, linked from the first page.
private const val REPO = "https://github.com/RoversX/ThinkTerm"

/// A first run's three pages, on the starry sky: what ThinkTerm is,
/// connecting a computer (or not yet), and in. Only while no host is kept
/// and they were never gone through; `enter` rushes into the app.
@Composable
fun Onboarding(store: HostStore, travel: (Int) -> Unit, enter: () -> Unit) {
    var page by remember { mutableIntStateOf(0) }
    // The sky moves with the pages, forward and back.
    LaunchedEffect(page) { travel(page) }
    val view = LocalView.current
    var tailnet by remember { mutableStateOf(false) }
    var adding by remember { mutableStateOf(false) }
    val sheet = tailnet || adding
    BackHandler(enabled = !sheet && page > 0) { page-- }
    // Those pages follow the phone, light or dark, and the system's bars
    // with them.
    LaunchedEffect(sheet) { LaunchSky.paged = sheet }
    DisposableEffect(Unit) { onDispose { LaunchSky.paged = false } }

    // A swipe to the right goes back a page; to the left, on from the first.
    Box(
        Modifier.fillMaxSize().pointerInput(sheet) {
            if (sheet) return@pointerInput
            var dx = 0f
            detectHorizontalDragGestures(
                onDragStart = { dx = 0f },
                onDragEnd = {
                    val far = 72.dp.toPx()
                    val next = when {
                        dx > far && page > 0 -> page - 1
                        dx < -far && page == 0 -> 1
                        else -> page
                    }
                    if (next != page) {
                        view.performHapticFeedback(HapticFeedbackConstants.CLOCK_TICK)
                        page = next
                    }
                },
            ) { change, amount ->
                change.consume()
                dx += amount
            }
        },
    ) {
        AnimatedContent(
            targetState = page,
            transitionSpec = {
                val forward = targetState > initialState
                (slideInHorizontally(tween(320)) { if (forward) it / 4 else -it / 4 } + fadeIn(tween(320)))
                    .togetherWith(slideOutHorizontally(tween(320)) { if (forward) -it / 4 else it / 4 } + fadeOut(tween(200)))
            },
            label = "onboarding",
        ) { shown ->
            Column(
                Modifier.fillMaxSize().navigationBarsPadding().padding(start = 28.dp, end = 28.dp, bottom = 34.dp),
                verticalArrangement = Arrangement.Bottom,
            ) {
                when (shown) {
                    0 -> Intro { page = 1 }
                    1 -> Connect(
                        tailscale = { tailnet = true },
                        manual = { adding = true },
                        later = { page = 2 },
                    )
                    else -> Welcome(enter)
                }
            }
        }

        // The app's own pages for the two ways in, over the sky; done with
        // either, the welcome comes next.
        AnimatedVisibility(
            visible = tailnet,
            enter = slideInVertically(tween(320)) { it } + fadeIn(),
            exit = slideOutVertically(tween(280)) { it } + fadeOut(),
        ) {
            BackHandler { tailnet = false; page = 2 }
            TailnetSettingsScreen(onBack = { tailnet = false; page = 2 })
        }
        AnimatedVisibility(
            visible = adding,
            enter = slideInVertically(tween(320)) { it } + fadeIn(),
            exit = slideOutVertically(tween(280)) { it } + fadeOut(),
        ) {
            BackHandler { adding = false }
            HostEditScreen(
                store = store,
                host = Host(),
                onSave = { host, secret, passphrase ->
                    store.saveEdit(host, secret, passphrase)
                    adding = false
                    page = 2
                },
                onCancel = { adding = false },
            )
        }
    }
}

/// The pages' ink, on the sky whatever the phone's appearance.
private val skyInk = Color(0xFFF2F2F7)
private val skyDim = Color(0xFFAEAEB2)

@Composable
private fun ColumnScope.Intro(next: () -> Unit) {
    val context = LocalContext.current
    Image(painterResource(R.drawable.onboarding_icon), contentDescription = null, modifier = Modifier.size(48.dp))
    Spacer(Modifier.height(14.dp))
    Text("ThinkTerm", color = skyInk, fontSize = 30.sp, fontWeight = FontWeight.Bold, letterSpacing = (-0.5).sp)
    Spacer(Modifier.height(6.dp))
    Text(tr("ob.tagline"), color = skyDim, fontSize = 15.sp, lineHeight = 22.sp)
    Spacer(Modifier.height(10.dp))
    Row(
        Modifier.clip(CircleShape).clickable {
            runCatching { context.startActivity(Intent(Intent.ACTION_VIEW, Uri.parse(REPO))) }
        }.padding(vertical = 4.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(6.dp),
    ) {
        Image(painterResource(R.drawable.ic_github), contentDescription = null, colorFilter = ColorFilter.tint(Color(0xFF8E8E93)), modifier = Modifier.size(13.dp))
        Text(REPO.removePrefix("https://"), color = Color(0xFF8E8E93), fontSize = 12.5.sp)
    }
    Spacer(Modifier.height(28.dp))
    PrimaryButton(tr("ob.continue"), next)
}

@Composable
private fun ColumnScope.Connect(tailscale: () -> Unit, manual: () -> Unit, later: () -> Unit) {
    Text(tr("ob.connect"), color = skyInk, fontSize = 25.sp, fontWeight = FontWeight.Bold, letterSpacing = (-0.4).sp)
    Spacer(Modifier.height(6.dp))
    Text(tr("ob.connect.body"), color = skyDim, fontSize = 14.sp, lineHeight = 21.sp)
    Spacer(Modifier.height(24.dp))
    PrimaryButton(tr("ob.tailscale"), tailscale)
    Spacer(Modifier.height(9.dp))
    SecondaryButton(tr("ob.manual"), manual)
    Spacer(Modifier.height(12.dp))
    Box(Modifier.align(Alignment.CenterHorizontally)) { SmallButton(tr("ob.later"), later) }
}

@Composable
private fun ColumnScope.Welcome(enter: () -> Unit) {
    Text(tr("ob.welcome"), color = skyInk, fontSize = 34.sp, fontWeight = FontWeight.Bold, letterSpacing = (-0.6).sp)
    Spacer(Modifier.height(28.dp))
    PrimaryButton(tr("ob.start"), enter, haptic = HapticFeedbackConstants.CONFIRM)
}

/// A press felt through the phone's own haptics, as the system's buttons
/// are: the platform's feedback, so the user's touch settings hold.
@Composable
private fun felt(haptic: Int, onClick: () -> Unit): () -> Unit {
    val view = LocalView.current
    return {
        view.performHapticFeedback(haptic)
        onClick()
    }
}

/// Glass over the sky: clear, so the stars behind bend at its edges, lit
/// along its rim; the lead button a little brighter than the rest.
private fun Modifier.skyGlass(sky: com.kyant.backdrop.Backdrop?, lead: Boolean): Modifier {
    if (sky == null) return clip(CircleShape).background(Color.White.copy(alpha = if (lead) .2f else .12f))
    return drawBackdrop(
        backdrop = sky,
        shape = { CircleShape },
        effects = {
            vibrancy()
            blur(2.dp.toPx())
            lens(16.dp.toPx(), 32.dp.toPx())
        },
        highlight = { Highlight.Default },
        onDrawSurface = { drawRect(Color.White.copy(alpha = if (lead) .16f else .07f)) },
    ).clip(CircleShape)
}

/// The lead button: glass a little brighter than the rest, over the sky.
@Composable
private fun PrimaryButton(label: String, onClick: () -> Unit, haptic: Int = HapticFeedbackConstants.VIRTUAL_KEY) {
    Box(
        Modifier.fillMaxWidth().widthIn(max = 420.dp).height(52.dp)
            .skyGlass(LocalGlass.current, lead = true)
            .clickable(onClick = felt(haptic, onClick)),
        contentAlignment = Alignment.Center,
    ) {
        Text(label, color = skyInk, fontSize = 16.sp, fontWeight = FontWeight.SemiBold)
    }
}

/// The other choices: clear glass.
@Composable
private fun SecondaryButton(label: String, onClick: () -> Unit) {
    Box(
        Modifier.fillMaxWidth().widthIn(max = 420.dp).height(52.dp)
            .skyGlass(LocalGlass.current, lead = false)
            .clickable(onClick = felt(HapticFeedbackConstants.VIRTUAL_KEY, onClick)),
        contentAlignment = Alignment.Center,
    ) {
        Text(label, color = skyInk, fontSize = 16.sp, fontWeight = FontWeight.SemiBold)
    }
}

/// A small glass capsule, as wide as its words.
@Composable
private fun SmallButton(label: String, onClick: () -> Unit) {
    Box(
        Modifier.height(38.dp)
            .skyGlass(LocalGlass.current, lead = false)
            .clickable(onClick = felt(HapticFeedbackConstants.VIRTUAL_KEY, onClick))
            .padding(horizontal = 16.dp),
        contentAlignment = Alignment.Center,
    ) {
        Text(label, color = Color(0xFFC7C7CC), fontSize = 13.5.sp)
    }
}
