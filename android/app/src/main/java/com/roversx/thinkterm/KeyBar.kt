package com.roversx.thinkterm

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp

/// What a key sends. Sticky Ctrl and Alt stay pressed until the next key
/// or the next character spends them, as on iOS.
private sealed interface Cap {
    val label: String

    data class Dom(override val label: String, val name: String, val ctrl: Boolean = false) : Cap
    data class Txt(override val label: String) : Cap { val text = label }
    data class Sticky(override val label: String, val ctrl: Boolean) : Cap
}

/// The keys a soft keyboard has not got.
private val bar: List<Cap> = listOf(
    Cap.Dom("esc", "Escape"),
    Cap.Dom("tab", "Tab"),
    Cap.Sticky("ctrl", ctrl = true),
    Cap.Sticky("alt", ctrl = false),
    Cap.Dom("↑", "ArrowUp"),
    Cap.Dom("↓", "ArrowDown"),
    Cap.Dom("←", "ArrowLeft"),
    Cap.Dom("→", "ArrowRight"),
    Cap.Dom("home", "Home"),
    Cap.Dom("end", "End"),
    Cap.Dom("pgup", "PageUp"),
    Cap.Dom("pgdn", "PageDown"),
    Cap.Txt("-"),
    Cap.Txt("/"),
    Cap.Txt("|"),
    Cap.Txt("~"),
    Cap.Dom("^C", "c", ctrl = true),
    Cap.Dom("^D", "d", ctrl = true),
    Cap.Dom("^L", "l", ctrl = true),
    Cap.Dom("^Z", "z", ctrl = true),
    Cap.Dom("⌫", "Backspace"),
)

@Composable
fun KeyBar(model: TerminalModel, modifier: Modifier = Modifier) {
    Row(
        modifier = modifier
            .fillMaxWidth()
            .height(40.dp)
            .horizontalScroll(rememberScrollState())
            .padding(horizontal = 6.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(6.dp),
    ) {
        for (cap in bar) {
            val held = when (cap) {
                is Cap.Sticky -> if (cap.ctrl) model.ctrlSticky else model.altSticky
                else -> false
            }
            Text(
                text = cap.label,
                color = Color.White,
                fontSize = 13.sp,
                fontFamily = FontFamily.Monospace,
                modifier = Modifier
                    .clip(RoundedCornerShape(6.dp))
                    .background(if (held) Color(0xFF3B6EA5) else Color.White.copy(alpha = 0.10f))
                    .clickable { send(cap, model) }
                    .padding(horizontal = 11.dp, vertical = 7.dp),
            )
        }
    }
}

private fun send(cap: Cap, model: TerminalModel) {
    when (cap) {
        is Cap.Dom -> model.key(cap.name, ctrl = cap.ctrl)
        is Cap.Txt -> model.text(cap.text)
        is Cap.Sticky -> if (cap.ctrl) model.ctrlSticky = !model.ctrlSticky else model.altSticky = !model.altSticky
    }
}
