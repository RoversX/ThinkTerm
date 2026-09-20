package com.roversx.thinkterm

import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Check
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.automirrored.filled.KeyboardArrowRight
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp

/// The App's context menu for something (a thread, a tab, a pane, a
/// project, the space), as a dropdown a long press opens: its items,
/// checks, separators and submenus (a submenu takes over the dropdown,
/// with a row that goes back). Built when the menu shows.
@Composable
fun AppContextMenu(
    model: TerminalModel,
    kind: String,
    id: String,
    expanded: Boolean,
    onDismiss: () -> Unit,
    /// Rows the shell adds above the App's own (already dropdown items).
    extra: @Composable (() -> Unit)? = null,
) {
    var items by remember(kind, id) { mutableStateOf<List<MenuItem>>(emptyList()) }
    var stack by remember(kind, id) { mutableStateOf<List<MenuItem>>(emptyList()) }
    if (expanded && items.isEmpty()) {
        items = model.contextMenu(kind, id)
    }
    DropdownMenu(expanded = expanded, onDismissRequest = { stack = emptyList(); onDismiss() }) {
        val parent = stack.lastOrNull()
        if (parent == null && extra != null) {
            extra()
            HorizontalDivider()
        }
        if (parent != null) {
            DropdownMenuItem(
                text = { Text(parent.label) },
                leadingIcon = { Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = null) },
                onClick = { stack = stack.dropLast(1) },
            )
            HorizontalDivider()
        }
        for (item in parent?.submenu ?: items) {
            MenuRow(item, onOpen = { stack = stack + it }) {
                model.menuAction(item.id)
                stack = emptyList()
                items = emptyList()
                onDismiss()
            }
        }
    }
}

@Composable
private fun MenuRow(item: MenuItem, onOpen: (MenuItem) -> Unit, onRun: () -> Unit) {
    when (item.kind) {
        "separator" -> HorizontalDivider()
        "header" -> Text(
            item.label,
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            modifier = Modifier.padding(horizontal = 16.dp, vertical = 6.dp),
        )
        else -> DropdownMenuItem(
            text = { Text(item.label) },
            enabled = item.enabled,
            leadingIcon = if (item.checked) ({ Icon(Icons.Default.Check, contentDescription = null) }) else null,
            trailingIcon = if (item.submenu.isNotEmpty()) ({
                Row { Spacer(Modifier.width(8.dp)); Icon(Icons.AutoMirrored.Filled.KeyboardArrowRight, contentDescription = null) }
            }) else null,
            onClick = { if (item.submenu.isNotEmpty()) onOpen(item) else onRun() },
        )
    }
}

/// The colour a thread's state dot takes, as on the desktop and iOS.
fun threadColor(status: String, live: Boolean): androidx.compose.ui.graphics.Color = when (status) {
    "Running" -> androidx.compose.ui.graphics.Color(0xFF34C759)
    "NeedsAttention" -> androidx.compose.ui.graphics.Color(0xFFFF9F0A)
    "Done" -> androidx.compose.ui.graphics.Color(0xFF0A84FF)
    else -> if (live) androidx.compose.ui.graphics.Color(0xFF8E8E93) else androidx.compose.ui.graphics.Color(0x668E8E93)
}
