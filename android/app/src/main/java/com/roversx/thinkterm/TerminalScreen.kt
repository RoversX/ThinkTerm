package com.roversx.thinkterm

import androidx.activity.compose.BackHandler
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.safeDrawingPadding
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.viewinterop.AndroidView

/// The screen that shows one host: the tab strip, the terminal surface,
/// a status line and the key bar. One model per connection; leaving
/// disconnects and stops the core.
@Composable
fun TerminalScreen(host: Host, store: HostStore, onBack: () -> Unit) {
    val context = LocalContext.current
    val model = remember(host.id) { TerminalModel(context, store, host) }

    // The connection does not wait for the surface: the core attaches to
    // a pane when both have arrived, in whichever order they come.
    LaunchedEffect(model) { model.connect() }

    DisposableEffect(model) {
        onDispose { model.shutdown() }
    }

    BackHandler {
        model.disconnect()
        onBack()
    }

    Column(
        Modifier
            .fillMaxSize()
            .background(model.background)
            // targetSdk 36 draws edge to edge: keep the chrome out from
            // under the status bar, and let the key bar ride the keyboard.
            .safeDrawingPadding()
            .imePadding()
    ) {
        TopBar(model, onBack = {
            model.disconnect()
            onBack()
        })
        TabStrip(model)
        Box(Modifier.fillMaxWidth().weight(1f)) {
            AndroidView(
                factory = { TerminalHostView(it, model) },
                modifier = Modifier.fillMaxSize(),
            )
        }
        StatusLine(model)
        KeyBar(model)
    }
}

@Composable
private fun TopBar(model: TerminalModel, onBack: () -> Unit) {
    Row(
        Modifier
            .fillMaxWidth()
            .height(44.dp)
            .padding(horizontal = 10.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(
            "‹",
            color = Color.White,
            fontSize = 24.sp,
            modifier = Modifier.clickable(onClick = onBack).padding(end = 12.dp),
        )
        Text(
            model.title.ifEmpty { model.host.display },
            color = Color.White,
            fontSize = 15.sp,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
            modifier = Modifier.weight(1f),
        )
    }
}

@Composable
private fun TabStrip(model: TerminalModel) {
    val tabs = model.tabs?.tabs ?: emptyList()
    Row(
        Modifier
            .fillMaxWidth()
            .height(34.dp)
            .horizontalScroll(rememberScrollState())
            .padding(horizontal = 8.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(6.dp),
    ) {
        for (tab in tabs) {
            Text(
                text = tab.label.ifEmpty { "Tab ${tab.tab}" },
                color = if (tab.current) Color.White else Color.White.copy(alpha = 0.62f),
                fontSize = 12.sp,
                fontWeight = if (tab.current) FontWeight.SemiBold else FontWeight.Normal,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
                modifier = Modifier
                    .clip(RoundedCornerShape(50))
                    .background(Color.White.copy(alpha = if (tab.current) 0.18f else 0.06f))
                    .clickable { model.chromeClick("pane", pane = tab.target) }
                    .widthIn(min = 70.dp, max = 150.dp)
                    .padding(horizontal = 10.dp, vertical = 6.dp),
            )
        }
        Text(
            "+",
            color = Color.White.copy(alpha = 0.62f),
            fontSize = 16.sp,
            modifier = Modifier
                .clickable { model.chromeClick("new-tab") }
                .padding(horizontal = 10.dp, vertical = 4.dp),
        )
    }
}

/// The connection's own line while it is not yet a pane, and whatever
/// the App wants said on top of it.
@Composable
private fun StatusLine(model: TerminalModel) {
    val toast = model.status?.toast
    val connection = model.connection.takeIf { it.isNotEmpty() && !it.startsWith("pane ") }
    val line = toast ?: connection ?: return
    Text(
        line,
        color = Color.White.copy(alpha = 0.7f),
        fontSize = 11.sp,
        maxLines = 2,
        modifier = Modifier
            .fillMaxWidth()
            .padding(horizontal = 10.dp, vertical = 4.dp),
    )
}
