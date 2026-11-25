package com.roversx.thinkterm

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.ScrollState
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.defaultMinSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.East
import androidx.compose.material.icons.filled.GridView
import androidx.compose.material.icons.filled.KeyboardArrowDown
import androidx.compose.material.icons.filled.KeyboardHide
import androidx.compose.material.icons.filled.North
import androidx.compose.material.icons.filled.South
import androidx.compose.material.icons.filled.West
import androidx.compose.material.icons.outlined.Keyboard
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.drawWithContent
import androidx.compose.ui.graphics.BlendMode
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.CompositingStrategy
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.kyant.backdrop.Backdrop

/// The keys a soft keyboard has not got, with sticky Ctrl and Alt, in
/// pieces of glass under the terminal whether the keyboard is up or not:
/// the keys, at their right end -- fixed, never scrolling away -- the
/// button that opens the extension panel underneath, and on its own the
/// keyboard's. The panel is the rest of the keys, the snippets, the
/// history and the colours. From ios KeyBar.swift.
@Composable
fun KeyBar(
    model: TerminalModel,
    keyboardUp: Boolean,
    glass: Backdrop?,
    onOpenSettings: () -> Unit,
    modifier: Modifier = Modifier,
    /// The device has a keyboard of its own: its keys are not drawn again,
    /// nor the button for the one on screen; the panel and the paperclip
    /// stay.
    hardKeyboard: Boolean = false,
) {
    val context = LocalContext.current
    val history = remember(context) { KeyHistory.get(context) }
    val tap = rememberKeyTap(model)
    var tab by remember { mutableStateOf(KeyPanelTab.KEYS) }
    val chrome = LocalChrome.current
    val scroll = rememberScrollState()

    // The keyboard coming up takes the panel's place: the two never stack.
    LaunchedEffect(keyboardUp) {
        if (keyboardUp) model.panelOpen = false
    }

    Column(modifier.fillMaxWidth(), verticalArrangement = Arrangement.spacedBy(8.dp)) {
        // Keys wide enough to hit, with room between them; what does not
        // fit beside the buttons scrolls, and the bar fades where it does.
        Row(
            Modifier.fillMaxWidth().padding(horizontal = 10.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            if (hardKeyboard) Spacer(Modifier.weight(1f))
            Row(
                (if (hardKeyboard) Modifier else Modifier.weight(1f))
                    .height(BAR_HEIGHT)
                    .glass(glass, chrome.dark, CircleShape),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                if (!hardKeyboard) Row(
                    Modifier
                        .weight(1f)
                        .fadeAtEnds(scroll)
                        .horizontalScroll(scroll)
                        .padding(horizontal = 4.dp),
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.spacedBy(KEY_GAP),
                ) {
                    for ((i, cap) in KeyCaps.bar.withIndex()) {
                        if (i == KeyCaps.BAR_LINE_BEFORE) BarLine()
                        BarKey(cap, held = isHeld(cap, model)) {
                            KeyCaps.send(cap, model, history, tap)
                        }
                    }
                }
                MoreButton(open = model.panelOpen) {
                    // Opening the panel puts the keyboard away: the panel is
                    // its stand-in, not a shelf on top of it.
                    if (!model.panelOpen) model.onHideKeyboard?.invoke()
                    model.panelOpen = !model.panelOpen
                    tap()
                }
            }
            AttachButton(model, glass, BUTTON_WIDTH, BAR_HEIGHT, tap)
            // The keyboard, up or away: on its own beside the keys.
            if (!hardKeyboard) Box(
                Modifier
                    .size(BUTTON_WIDTH, BAR_HEIGHT)
                    .glass(glass, chrome.dark, CircleShape)
                    .clickable {
                        if (keyboardUp) {
                            model.onHideKeyboard?.invoke()
                        } else {
                            model.panelOpen = false
                            model.onShowKeyboard?.invoke()
                        }
                        tap()
                    },
                contentAlignment = Alignment.Center,
            ) {
                Icon(
                    if (keyboardUp) Icons.Filled.KeyboardHide else Icons.Outlined.Keyboard,
                    contentDescription = tr(if (keyboardUp) "keyboard.hide" else "keyboard.show"),
                    tint = ink,
                    modifier = Modifier.size(20.dp),
                )
            }
        }
        if (model.panelOpen) {
            Box(Modifier.background(model.background)) {
                KeyPanel(model, tab, { tab = it }, onOpenSettings)
            }
        }
    }
}

/// A piece's height: the keys' capsule and the buttons beside it.
private val BAR_HEIGHT = 46.dp
/// A button's width: as wide as it is high, round, as on the iPhone.
private val BUTTON_WIDTH = BAR_HEIGHT
/// Between two keys, besides each key's own padding.
private val KEY_GAP = 2.dp
/// How far in from an end the keys fade while more lie past it.
private val KEY_FADE = 28.dp

/// The keys fade out at an end of the bar while more of them lie past
/// it: as strongly as there is left to scroll, up to the fade's width, so
/// it comes in with the scroll and is gone at the end.
private fun Modifier.fadeAtEnds(scroll: ScrollState): Modifier = this
    .graphicsLayer { compositingStrategy = CompositingStrategy.Offscreen }
    .drawWithContent {
        drawContent()
        val fade = KEY_FADE.toPx()
        if (size.width <= fade * 2) return@drawWithContent
        val before = (scroll.value / fade).coerceIn(0f, 1f)
        val after = ((scroll.maxValue - scroll.value) / fade).coerceIn(0f, 1f)
        if (before == 0f && after == 0f) return@drawWithContent
        val inEdge = fade / size.width
        drawRect(
            Brush.horizontalGradient(
                0f to Color.Black.copy(alpha = 1f - before),
                inEdge to Color.Black,
                1f - inEdge to Color.Black,
                1f to Color.Black.copy(alpha = 1f - after),
            ),
            blendMode = BlendMode.DstIn,
        )
    }

/// The thin upright line between two groups of keys.
@Composable
private fun BarLine() {
    Box(
        Modifier
            .padding(horizontal = 4.dp)
            .size(width = 1.dp, height = 20.dp)
            .background(ink.copy(alpha = 0.25f)),
    )
}

/// A sticky modifier shows as pressed until the next key spends it.
private fun isHeld(cap: KeyCap, model: TerminalModel): Boolean = when {
    cap !is KeyCap.Sticky -> false
    cap.mod == KeyCap.Mod.CTRL -> model.ctrlSticky
    else -> model.altSticky
}

@Composable
private fun BarKey(cap: KeyCap, held: Boolean, onClick: () -> Unit) {
    Box(
        Modifier
            .defaultMinSize(minWidth = 34.dp, minHeight = 34.dp)
            .clip(CircleShape)
            .background(if (held) LeadBlue else Color.Transparent)
            .clickable(onClick = onClick)
            .padding(horizontal = 7.dp),
        contentAlignment = Alignment.Center,
    ) {
        val arrow = arrowIcon(cap.label)
        if (arrow != null) Icon(
            arrow,
            contentDescription = cap.label,
            tint = ink,
            modifier = Modifier.size(18.dp),
        ) else Text(
            cap.label,
            color = if (held) Color.White else ink,
            fontSize = 14.sp,
            fontFamily = FontFamily.Monospace,
            maxLines = 1,
            softWrap = false,
        )
    }
}

/// An arrow key drawn as an icon: the monospace font has no arrows, and
/// the fallback's are small beside the letters.
private fun arrowIcon(label: String): ImageVector? = when (label) {
    "←" -> Icons.Filled.West
    "↑" -> Icons.Filled.North
    "↓" -> Icons.Filled.South
    "→" -> Icons.Filled.East
    else -> null
}

@Composable
private fun MoreButton(open: Boolean, onClick: () -> Unit) {
    Box(
        Modifier
            .padding(start = 2.dp, end = 5.dp)
            .size(34.dp)
            .clip(CircleShape)
            .background(if (open) LeadBlue else ink.copy(alpha = 0.12f))
            .clickable(onClick = onClick),
        contentAlignment = Alignment.Center,
    ) {
        Icon(
            if (open) Icons.Filled.KeyboardArrowDown else Icons.Filled.GridView,
            contentDescription = tr("p.panel"),
            tint = if (open) Color.White else ink,
            modifier = Modifier.size(18.dp),
        )
    }
}
