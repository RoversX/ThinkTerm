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

/// The desktop's sidebar and tab strip in one sheet, the way both are on
/// screen at once there: the Space, its threads by project, under an
/// opened thread its tabs, and under a tab with several panes, its panes.
/// Everything is named as the desktop names it: thread, tab, pane.
@Composable
fun TreeSheet(model: TerminalModel, onDismiss: () -> Unit) {
    val sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true)
    val open = remember { TreeOpen() }
    var spaceMenu by remember { mutableStateOf<List<MenuItem>>(emptyList()) }
    var spacesShown by remember { mutableStateOf(false) }
    var renaming by remember { mutableStateOf<Pair<String, String>?>(null) }
    var renameText by remember { mutableStateOf("") }
    val threads = model.threads?.threads ?: emptyList()
    val rows = model.sidebar?.rows ?: emptyList()
    val error = model.sidebar?.newProjectError
    val editing = model.sidebar?.editing

    LaunchedEffect(Unit) {
        model.refreshViews()
        spaceMenu = model.contextMenu("space", "")
        // The current thread starts open: its tabs are what the strip shows.
        model.threads?.current?.let { if (!open.threads.contains(it.id)) open.threads.add(it.id) }
    }
    LaunchedEffect(editing) {
        val id = editing?.id
        if (editing == null || editing.kind == "none" || id == null || renaming != null) return@LaunchedEffect
        renameText = ""
        renaming = editing.kind to id
    }

    ModalBottomSheet(
        onDismissRequest = onDismiss,
        sheetState = sheetState,
        modifier = Modifier.fillMaxSize(),
    ) {
        Row(
            Modifier.fillMaxWidth().padding(start = 8.dp, end = 4.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            TextButton(onClick = onDismiss) { Text(tr("done")) }
            Spacer(Modifier.width(8.dp))
            Text(tr("threads"), style = MaterialTheme.typography.titleMedium)
            Spacer(Modifier.weight(1f))
            Box {
                IconButton(onClick = {
                    spaceMenu = model.contextMenu("space", "")
                    spacesShown = true
                }) {
                    Icon(Icons.Default.Layers, contentDescription = tr("workspaces"))
                }
                DropdownMenu(expanded = spacesShown, onDismissRequest = { spacesShown = false }) {
                    for (item in spaceMenu) {
                        if (item.kind != "item") continue
                        DropdownMenuItem(
                            text = { Text(item.label) },
                            enabled = item.enabled,
                            leadingIcon = if (item.checked) ({ Icon(Icons.Default.Check, contentDescription = null) }) else null,
                            onClick = {
                                model.menuAction(item.id)
                                spaceMenu = model.contextMenu("space", "")
                                spacesShown = false
                            },
                        )
                    }
                }
            }
            IconButton(onClick = { model.chromeClick("new-tab") }) {
                Icon(Icons.Default.Add, contentDescription = tr("thread.new"))
            }
        }
        LazyColumn(Modifier.fillMaxWidth().weight(1f)) {
            items(rows) { row -> SideRowView(model, row, threads, open, onDismiss) }
            if (!error.isNullOrEmpty()) {
                item {
                    Text(
                        error,
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.error,
                        modifier = Modifier.padding(horizontal = 16.dp, vertical = 8.dp),
                    )
                }
            }
        }
    }

    if (renaming != null) {
        AlertDialog(
            onDismissRequest = {
                model.sideKey("Escape", "")
                renaming = null
            },
            title = { Text(tr("rename")) },
            text = {
                OutlinedTextField(
                    value = renameText,
                    onValueChange = { renameText = it },
                    singleLine = true,
                    label = { Text(tr("f.name")) },
                )
            },
            confirmButton = {
                TextButton(onClick = {
                    model.sideKey("Enter", renameText)
                    renaming = null
                }) { Text(tr("save")) }
            },
            dismissButton = {
                TextButton(onClick = {
                    model.sideKey("Escape", "")
                    renaming = null
                }) { Text(tr("cancel")) }
            },
        )
    }
}

/// Which threads and tabs are unfolded, for as long as the sheet is up.
private class TreeOpen {
    val threads = mutableStateListOf<String>()
    val tabs = mutableStateListOf<String>()

    fun toggleThread(id: String) { if (!threads.remove(id)) threads.add(id) }

    fun toggleTab(key: String) { if (!tabs.remove(key)) tabs.add(key) }
}

@Composable
private fun SideRowView(
    model: TerminalModel,
    row: SideRow,
    threads: List<ThreadView>,
    open: TreeOpen,
    onDismiss: () -> Unit,
) {
    val accent = MaterialTheme.colorScheme.primary
    val secondary = MaterialTheme.colorScheme.onSurfaceVariant
    when (row) {
        is SideRow.Space -> Text(
            row.name,
            style = MaterialTheme.typography.titleMedium,
            modifier = Modifier.padding(start = 16.dp, end = 16.dp, top = 10.dp, bottom = 4.dp),
        )
        is SideRow.NewThread -> Row(
            Modifier
                .fillMaxWidth()
                .clickable { model.sideClick("new-thread") }
                .padding(horizontal = 16.dp, vertical = 10.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            Icon(Icons.Default.Add, contentDescription = null, tint = accent, modifier = Modifier.size(18.dp))
            Text(tr("thread.new"), color = accent)
        }
        is SideRow.Pinned -> TreeCaption(tr("pinned"))
        is SideRow.Workspaces -> TreeCaption(tr("workspaces"))
        is SideRow.Others -> TreeCaption(tr("otherwindows"))
        is SideRow.Project -> ProjectRow(model, row)
        is SideRow.Thread -> {
            val thread = threads.firstOrNull { it.id == row.row.id }
            if (thread == null) {
                Text(row.row.name, modifier = Modifier.padding(horizontal = 16.dp, vertical = 8.dp))
            } else {
                ThreadRows(model, thread, if (row.row.project.isEmpty()) 0.dp else 14.dp, open, onDismiss)
            }
        }
        is SideRow.Archived -> Row(
            Modifier
                .fillMaxWidth()
                .clickable { model.sideClick("toggle-archived", flag = !row.open) }
                .padding(horizontal = 16.dp, vertical = 8.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(6.dp),
        ) {
            Icon(
                Icons.AutoMirrored.Filled.KeyboardArrowRight,
                contentDescription = null,
                tint = secondary,
                modifier = Modifier.size(14.dp).rotate(if (row.open) 90f else 0f),
            )
            Text("${row.label} (${row.count})", style = MaterialTheme.typography.bodySmall, color = secondary)
        }
        is SideRow.Window -> Row(
            Modifier
                .fillMaxWidth()
                .background(if (row.selected) accent.copy(alpha = 0.2f) else Color.Transparent)
                .clickable { model.sideClick("window", row.id.toString()) }
                .padding(horizontal = 16.dp, vertical = 8.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            Icon(Icons.Default.Window, contentDescription = null, tint = secondary, modifier = Modifier.size(16.dp))
            Text(
                row.title.ifEmpty { tr("window.n", row.id) },
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
        }
    }
}

@Composable
private fun TreeCaption(text: String) {
    Text(
        text.uppercase(),
        style = MaterialTheme.typography.labelSmall,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
        modifier = Modifier.padding(start = 16.dp, end = 16.dp, top = 12.dp, bottom = 2.dp),
    )
}

@Composable
private fun ProjectRow(model: TerminalModel, row: SideRow.Project) {
    val accent = MaterialTheme.colorScheme.primary
    val secondary = MaterialTheme.colorScheme.onSurfaceVariant
    var menu by remember { mutableStateOf(false) }
    Box {
        Row(
            Modifier
                .fillMaxWidth()
                .combinedClickable(
                    onClick = { model.sideClick("toggle-project", row.id) },
                    onLongClick = { menu = true },
                )
                .padding(start = 8.dp, end = 8.dp, top = 4.dp, bottom = 4.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            IconButton(onClick = { model.sideClick("toggle-project", row.id) }, modifier = Modifier.size(28.dp)) {
                Icon(
                    Icons.AutoMirrored.Filled.KeyboardArrowRight,
                    contentDescription = null,
                    tint = secondary,
                    modifier = Modifier.size(16.dp).rotate(if (row.collapsed) 0f else 90f),
                )
            }
            Column(Modifier.weight(1f).padding(start = 4.dp)) {
                Text(
                    row.name,
                    style = MaterialTheme.typography.titleSmall,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                Text(
                    row.path,
                    style = MaterialTheme.typography.labelSmall,
                    color = secondary,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
            if (!row.archived) {
                IconButton(onClick = { model.sideClick("new-thread", row.id) }, modifier = Modifier.size(28.dp)) {
                    Icon(Icons.Default.Add, contentDescription = null, tint = accent, modifier = Modifier.size(18.dp))
                }
            }
        }
        AppContextMenu(
            model = model,
            kind = if (row.archived) "archived-project" else "project",
            id = row.id,
            expanded = menu,
            onDismiss = { menu = false },
        )
    }
}

/// A thread, and when open, its tabs under it.
@Composable
private fun ThreadRows(
    model: TerminalModel,
    thread: ThreadView,
    indent: Dp,
    open: TreeOpen,
    onDismiss: () -> Unit,
) {
    val accent = MaterialTheme.colorScheme.primary
    val secondary = MaterialTheme.colorScheme.onSurfaceVariant
    val opened = open.threads.contains(thread.id)
    var menu by remember { mutableStateOf(false) }
    Column {
        Box {
            Row(
                Modifier
                    .fillMaxWidth()
                    .background(if (thread.current) accent.copy(alpha = 0.18f) else Color.Transparent)
                    .combinedClickable(
                        onClick = {
                            model.sideClick("thread", thread.id)
                            onDismiss()
                        },
                        onLongClick = { menu = true },
                    )
                    .padding(start = 16.dp + indent, end = 8.dp, top = 6.dp, bottom = 6.dp),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                Box(Modifier.size(8.dp).background(threadColor(thread.status, thread.live), CircleShape))
                Text(
                    thread.name,
                    fontWeight = if (thread.unread) FontWeight.SemiBold else FontWeight.Normal,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                    modifier = Modifier.weight(1f),
                )
                if (thread.pinned) {
                    Icon(Icons.Default.PushPin, contentDescription = null, tint = secondary, modifier = Modifier.size(14.dp))
                }
                Text(
                    if (thread.live) tr(if (thread.tabs.size == 1) "tab.one" else "tab.n", thread.tabs.size) else tr("thread.off"),
                    style = MaterialTheme.typography.labelSmall,
                    color = secondary,
                )
                if (thread.tabs.isNotEmpty()) {
                    IconButton(onClick = { open.toggleThread(thread.id) }, modifier = Modifier.size(28.dp)) {
                        Icon(
                            Icons.Default.KeyboardArrowDown,
                            contentDescription = null,
                            tint = secondary,
                            modifier = Modifier.size(18.dp).rotate(if (opened) 0f else -90f),
                        )
                    }
                }
            }
            AppContextMenu(model = model, kind = "thread", id = thread.id, expanded = menu, onDismiss = { menu = false })
        }
        if (opened) {
            for (tab in thread.tabs) TabRows(model, thread, tab, indent + 20.dp, open, onDismiss)
        }
    }
}

/// A tab, and when open, its panes.
@Composable
private fun TabRows(
    model: TerminalModel,
    thread: ThreadView,
    tab: ThreadTab,
    indent: Dp,
    open: TreeOpen,
    onDismiss: () -> Unit,
) {
    val accent = MaterialTheme.colorScheme.primary
    val secondary = MaterialTheme.colorScheme.onSurfaceVariant
    val key = thread.id + ":" + tab.tab
    val opened = open.tabs.contains(key)
    val current = thread.current && tab.current
    var menu by remember { mutableStateOf(false) }
    Box {
        Row(
            Modifier
                .fillMaxWidth()
                .combinedClickable(
                    onClick = {
                        model.chromeClick("pane", pane = tab.target)
                        onDismiss()
                    },
                    onLongClick = { menu = true },
                )
                .padding(start = 16.dp + indent, end = 8.dp, top = 5.dp, bottom = 5.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            Icon(
                Icons.Default.Terminal,
                contentDescription = null,
                tint = if (current) accent else secondary,
                modifier = Modifier.size(16.dp),
            )
            Row(
                Modifier.weight(1f),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(6.dp),
            ) {
                Text(
                    tab.title.ifEmpty { tr("tab.num", tab.tab) },
                    style = MaterialTheme.typography.bodyMedium,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                val first = tab.panes.firstOrNull()
                if (first != null && first.title.isNotEmpty() && first.title != tab.title) {
                    Text(
                        first.title,
                        style = MaterialTheme.typography.labelSmall,
                        color = secondary,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                }
            }
            if (current) {
                Icon(Icons.Default.Check, contentDescription = null, tint = accent, modifier = Modifier.size(16.dp))
            }
            if (tab.panes.size > 1) {
                Text(tr("panes.n", tab.panes.size), style = MaterialTheme.typography.labelSmall, color = secondary)
                IconButton(onClick = { open.toggleTab(key) }, modifier = Modifier.size(28.dp)) {
                    Icon(
                        Icons.Default.KeyboardArrowDown,
                        contentDescription = null,
                        tint = secondary,
                        modifier = Modifier.size(18.dp).rotate(if (opened) 0f else -90f),
                    )
                }
            }
        }
        AppContextMenu(model = model, kind = "tab", id = tab.tab.toString(), expanded = menu, onDismiss = { menu = false })
    }
    if (opened) {
        val focused = model.tabs?.current?.panes?.firstOrNull { it.current }?.pane
        for (pane in tab.panes) {
            Row(
                Modifier
                    .fillMaxWidth()
                    .clickable {
                        model.chromeClick("pane", pane = pane.pane)
                        onDismiss()
                    }
                    .padding(start = 16.dp + indent + 22.dp, end = 16.dp, top = 5.dp, bottom = 5.dp),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                Box(Modifier.size(6.dp).background(secondary, RoundedCornerShape(1.dp)))
                Text(
                    pane.title.ifEmpty { "shell" },
                    style = MaterialTheme.typography.bodyMedium,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                    modifier = Modifier.weight(1f),
                )
                if (thread.current && focused == pane.pane) {
                    Icon(Icons.Default.Check, contentDescription = null, tint = accent, modifier = Modifier.size(16.dp))
                }
            }
        }
    }
}

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
