package com.roversx.thinkterm

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
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
import androidx.compose.material.icons.filled.GridView
import androidx.compose.material.icons.filled.KeyboardArrowDown
import androidx.compose.material.icons.filled.KeyboardHide
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
import androidx.compose.ui.graphics.Color
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

    // The keyboard coming up takes the panel's place: the two never stack.
    LaunchedEffect(keyboardUp) {
        if (keyboardUp) model.panelOpen = false
    }

    Column(modifier.fillMaxWidth(), verticalArrangement = Arrangement.spacedBy(8.dp)) {
        // Tight enough that the usual eight keys show on a phone held
        // upright beside the buttons; more of them scroll.
        Row(
            Modifier.fillMaxWidth().padding(horizontal = 10.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(6.dp),
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
                        .horizontalScroll(rememberScrollState())
                        .padding(horizontal = 4.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    for (cap in KeyCaps.bar) {
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
/// A button's width: narrower than it is high, the room being the keys'.
private val BUTTON_WIDTH = 40.dp

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
            .defaultMinSize(minWidth = 26.dp, minHeight = 34.dp)
            .clip(CircleShape)
            .background(if (held) LeadBlue else Color.Transparent)
            .clickable(onClick = onClick)
            .padding(horizontal = 5.dp),
        contentAlignment = Alignment.Center,
    ) {
        Text(
            cap.label,
            color = if (held) Color.White else ink,
            fontSize = 13.sp,
            fontFamily = FontFamily.Monospace,
            maxLines = 1,
            softWrap = false,
        )
    }
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
