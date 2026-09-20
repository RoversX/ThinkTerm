@file:OptIn(ExperimentalFoundationApi::class, ExperimentalMaterial3Api::class)

package com.roversx.thinkterm

import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.gestures.calculateZoom
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.KeyboardArrowRight
import androidx.compose.material.icons.filled.Add
import androidx.compose.material.icons.filled.Check
import androidx.compose.material.icons.filled.KeyboardArrowDown
import androidx.compose.material.icons.filled.Layers
import androidx.compose.material.icons.filled.PushPin
import androidx.compose.material.icons.filled.Terminal
import androidx.compose.material.icons.filled.Window
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.rememberModalBottomSheetState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.clipToBounds
import androidx.compose.ui.draw.rotate
import androidx.compose.ui.geometry.Rect
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.input.pointer.PointerInputScope
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.layout.boundsInRoot
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.tween
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.layout.onGloballyPositioned
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import kotlinx.coroutines.delay

private const val OVERVIEW_ROWS = 9
private const val OVERVIEW_COLS = 38

/// The desktop's Live Overview as a layer under the terminal: a card per
/// thread, grouped by project, two to a row. The thread on show leaves
/// its thumbnail empty and reports the box, for the screen above to zoom
/// the live terminal into it.
@Composable
fun OverviewScreen(
    model: TerminalModel,
    liveThread: String?,
    livePreview: Boolean,
    onCardBounds: (CardFrames?) -> Unit,
    /// A tap on a card, with the card's frames, so the terminal can grow
    /// back out of that card; a pinch passes nothing.
    onDismiss: (ThreadView?, CardFrames?) -> Unit,
) {
    val threads = model.threads?.threads ?: emptyList()
    val groups = threads.groupBy { it.project }.toList()

    DisposableEffect(Unit) { onDispose { onCardBounds(null) } }
    // Every live thread's current tab, asked for its last rows.
    LaunchedEffect(Unit) {
        model.refreshViews()
        while (true) {
            for (thread in model.threads?.threads ?: emptyList()) {
                if (!thread.live) continue
                previewPane(thread)?.let { model.requestPreview(it, OVERVIEW_ROWS) }
            }
            delay(2000)
        }
    }

    Box(
        Modifier
            .fillMaxSize()
            .background(Color(0xFF0F0F0F))
            // Pinching out returns to the terminal.
            .pointerInput(Unit) { awaitPinchOut { onDismiss(null, null) } }
    ) {
        Column(
            Modifier
                .fillMaxSize()
                .verticalScroll(rememberScrollState())
                .padding(horizontal = 12.dp, vertical = 8.dp)
        ) {
            if (threads.isEmpty()) {
                Box(Modifier.fillMaxWidth().padding(top = 60.dp), contentAlignment = Alignment.TopCenter) {
                    Text(tr("overview.none"), color = overviewSecondary)
                }
            } else {
                for ((project, group) in groups) {
                    GroupHeader(model.threads?.space ?: "", project, group.size)
                    for (pair in group.chunked(2)) {
                        Row(
                            Modifier.fillMaxWidth().padding(bottom = 10.dp),
                            horizontalArrangement = Arrangement.spacedBy(10.dp),
                        ) {
                            for (thread in pair) {
                                ThreadCard(
                                    model = model,
                                    thread = thread,
                                    live = thread.id == liveThread,
                                    livePreview = livePreview,
                                    onCardBounds = onCardBounds,
                                    onDismiss = onDismiss,
                                    modifier = Modifier.weight(1f),
                                )
                            }
                            if (pair.size == 1) Spacer(Modifier.weight(1f))
                        }
                    }
                }
            }
        }
    }
}

private val overviewSecondary = Color.White.copy(alpha = 0.55f)

@Composable
private fun GroupHeader(space: String, project: String, count: Int) {
    Row(
        Modifier.fillMaxWidth().padding(top = 10.dp, bottom = 8.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(6.dp),
    ) {
        Text(space, fontSize = 12.sp, color = Color.White, maxLines = 1, overflow = TextOverflow.Ellipsis)
        Text("·", fontSize = 12.sp, color = overviewSecondary)
        Text(project, fontSize = 12.sp, color = Color.White, maxLines = 1, overflow = TextOverflow.Ellipsis)
        Box(Modifier.weight(1f).height(1.dp).background(Color.White.copy(alpha = 0.08f)))
        Text("$count", fontSize = 12.sp, color = overviewSecondary)
    }
}


/// Where the live card is and where its thumbnail is, in root pixels.
data class CardFrames(val card: Rect, val thumb: Rect)

/// A card's top: the thread's dot and name, its tabs as dots, the current
/// tab's title, and an offline badge. Shared with the screen's zoom,
/// which carries the same header while the terminal shrinks.
@Composable
fun ThreadCardHeader(thread: ThreadView, accent: Color, modifier: Modifier = Modifier) {
    val currentTab = thread.tabs.firstOrNull { it.current } ?: thread.tabs.firstOrNull()
    // One line: the dot and name, the current tab's title after them, and
    // an offline badge at the end; the tabs as dots took a row of their own.
    Row(
        modifier.fillMaxWidth().padding(start = 10.dp, end = 10.dp, top = 9.dp, bottom = 8.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(7.dp),
    ) {
        Box(Modifier.size(7.dp).background(threadColor(thread.status, thread.live), CircleShape))
        Text(
            thread.name,
            fontSize = 13.5.sp,
            fontWeight = FontWeight.SemiBold,
            color = Color.White,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
        )
        Text(
            currentTab?.title ?: "",
            fontSize = 10.sp,
            fontFamily = FontFamily.Monospace,
            color = overviewSecondary,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
            modifier = Modifier.weight(1f),
        )
        if (!thread.live) {
            Text(
                tr("offline"),
                fontSize = 9.sp,
                fontWeight = FontWeight.SemiBold,
                color = overviewSecondary,
                modifier = Modifier
                    .background(Color.Gray.copy(alpha = 0.22f), CircleShape)
                    .padding(horizontal = 6.dp, vertical = 2.dp),
            )
        }
    }
}

@Composable
private fun ThreadCard(
    model: TerminalModel,
    thread: ThreadView,
    live: Boolean,
    livePreview: Boolean,
    onCardBounds: (CardFrames?) -> Unit,
    onDismiss: (ThreadView?, CardFrames?) -> Unit,
    modifier: Modifier = Modifier,
) {
    val accent = MaterialTheme.colorScheme.primary
    val shape = RoundedCornerShape(13.dp)
    // Every card knows where it is and where its thumbnail is: the live
    // one reports it so the terminal can shrink into it, and any card
    // hands it over when tapped so the terminal can grow back out of it.
    var cardRect by remember { mutableStateOf<Rect?>(null) }
    var thumbRect by remember { mutableStateOf<Rect?>(null) }
    fun frames(): CardFrames? {
        val c = cardRect ?: return null
        val t = thumbRect ?: return null
        return CardFrames(c, t)
    }
    fun report() {
        if (live) frames()?.let(onCardBounds)
    }
    // The moving card draws its own border until it has landed.
    val border = when {
        !live -> Color.White.copy(alpha = 0.07f)
        livePreview -> accent
        else -> Color.Transparent
    }
    Column(
        modifier
            .onGloballyPositioned { cardRect = it.boundsInRoot(); report() }
            .clip(shape)
            .background(Color(0xFF1A1C21))
            .border(if (live) 2.dp else 1.dp, border, shape)
            .clickable {
                if (!live) model.sideClick("thread", thread.id)
                onDismiss(thread, frames())
            }
    ) {
        ThreadCardHeader(thread, accent)
        Box(
            Modifier
                .fillMaxWidth()
                .height(92.dp)
                .background(if (thread.live) model.background else Color.Black.copy(alpha = 0.4f))
                .clipToBounds()
        ) {
            // The live card's rows wait until the terminal has faded out of
            // it, so the two never show at once.
            val rowsAlpha by animateFloatAsState(if (!live || livePreview) 1f else 0f, tween(150), label = "rows")
            Text(
                previewText(model, thread),
                fontSize = 7.sp,
                lineHeight = 8.5.sp,
                fontFamily = FontFamily.Monospace,
                color = Color.White,
                modifier = Modifier.fillMaxWidth().padding(horizontal = 8.dp, vertical = 7.dp).graphicsLayer { alpha = rowsAlpha },
            )
            // The terminal itself lands over this, then fades into the same
            // preview as every other card; the box reports where.
            Box(Modifier.fillMaxSize().onGloballyPositioned { thumbRect = it.boundsInRoot(); report() })
        }
    }
}

private fun previewPane(thread: ThreadView): Int? =
    (thread.tabs.firstOrNull { it.current } ?: thread.tabs.firstOrNull())?.target

/// The rows fetched for the thread's pane in their colours, trailing
/// blank rows dropped, each cut at about a half-width card's worth.
private fun previewText(model: TerminalModel, thread: ThreadView): AnnotatedString {
    val pane = previewPane(thread) ?: return AnnotatedString("")
    val fetched = model.previews[pane] ?: return AnnotatedString("")
    val kept = fetched.dropLastWhile { it.runs.isEmpty() }.takeLast(OVERVIEW_ROWS)
    return buildAnnotatedString {
        for ((i, row) in kept.withIndex()) {
            var used = 0
            for (run in row.runs) {
                val room = OVERVIEW_COLS - used
                if (room <= 0) break
                val piece = run.text.take(room)
                withStyle(SpanStyle(color = parseHex(run.fg) ?: Color.White)) { append(piece) }
                used += piece.length
            }
            if (i < kept.size - 1) append("\n")
        }
    }
}

/// Two fingers spreading, the zoom multiplied over the gesture. One
/// finger is left alone, so the list under it still scrolls.
private suspend fun PointerInputScope.awaitPinchOut(onPinchOut: () -> Unit) {
    awaitEachGesture {
        awaitFirstDown(requireUnconsumed = false)
        var zoom = 1f
        var fired = false
        do {
            val event = awaitPointerEvent()
            if (event.changes.size >= 2) {
                zoom *= event.calculateZoom()
                if (!fired && zoom > 1.25f) {
                    fired = true
                    onPinchOut()
                }
            }
        } while (event.changes.any { it.pressed })
    }
}
