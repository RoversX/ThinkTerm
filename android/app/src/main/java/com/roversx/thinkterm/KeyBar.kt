package com.roversx.thinkterm

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.GridView
import androidx.compose.material.icons.filled.KeyboardArrowDown
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
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

/// The keys a soft keyboard has not got, with sticky Ctrl and Alt, and at
/// the row's right end — fixed, never scrolling away — the button that
/// opens the extension panel underneath, where the rest of the keys, the
/// snippets, the history and the colours live. From ios KeyBar.swift.
@Composable
fun KeyBar(
    model: TerminalModel,
    keyboardUp: Boolean,
    onOpenSettings: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val context = LocalContext.current
    val history = remember(context) { KeyHistory.get(context) }
    val tap = rememberKeyTap(model)
    var tab by remember { mutableStateOf(KeyPanelTab.KEYS) }

    // The keyboard coming up takes the panel's place: the two never stack.
    LaunchedEffect(keyboardUp) {
        if (keyboardUp) model.panelOpen = false
    }

    Column(modifier.fillMaxWidth().background(model.background)) {
        Row(
            Modifier.fillMaxWidth().height(38.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Row(
                Modifier
                    .weight(1f)
                    .horizontalScroll(rememberScrollState())
                    .padding(horizontal = 6.dp),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(6.dp),
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
        if (model.panelOpen) {
            KeyPanel(model, tab, { tab = it }, onOpenSettings)
        }
    }
}

/// A sticky modifier shows as pressed until the next key spends it.
private fun isHeld(cap: KeyCap, model: TerminalModel): Boolean = when {
    cap !is KeyCap.Sticky -> false
    cap.mod == KeyCap.Mod.CTRL -> model.ctrlSticky
    else -> model.altSticky
}

@Composable
private fun BarKey(cap: KeyCap, held: Boolean, onClick: () -> Unit) {
    Text(
        cap.label,
        color = Color.White,
        fontSize = 13.sp,
        fontFamily = FontFamily.Monospace,
        maxLines = 1,
        softWrap = false,
        modifier = Modifier
            .clip(RoundedCornerShape(6.dp))
            .background(if (held) MaterialTheme.colorScheme.primary else Color.White.copy(alpha = 0.08f))
            .clickable(onClick = onClick)
            .padding(horizontal = 10.dp, vertical = 6.dp),
    )
}

@Composable
private fun MoreButton(open: Boolean, onClick: () -> Unit) {
    Box(
        Modifier
            .padding(start = 4.dp, end = 6.dp)
            .width(40.dp)
            .height(28.dp)
            .clip(RoundedCornerShape(7.dp))
            .background(if (open) MaterialTheme.colorScheme.primary else Color.White.copy(alpha = 0.14f))
            .clickable(onClick = onClick),
        contentAlignment = Alignment.Center,
    ) {
        Icon(
            if (open) Icons.Filled.KeyboardArrowDown else Icons.Filled.GridView,
            contentDescription = tr("p.panel"),
            tint = Color.White,
            modifier = Modifier.height(18.dp).width(18.dp),
        )
    }
}
