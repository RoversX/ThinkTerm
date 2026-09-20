package com.roversx.thinkterm

import androidx.activity.compose.BackHandler
import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.spring
import androidx.compose.animation.core.tween
import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.gestures.detectDragGestures
import androidx.compose.foundation.gestures.detectTransformGestures
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxScope
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.ime
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawingPadding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.filled.Add
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.GridView
import androidx.compose.material.icons.filled.Warning
import androidx.compose.material.icons.filled.CheckCircle
import androidx.compose.material.icons.outlined.Circle
import androidx.compose.material.icons.outlined.PowerOff
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.TextButton
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.runtime.snapshotFlow
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.drawWithContent
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Rect
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.TransformOrigin
import androidx.compose.ui.graphics.drawscope.clipRect
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.graphics.lerp
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.layout.boundsInRoot
import androidx.compose.ui.layout.onGloballyPositioned
import androidx.compose.ui.layout.positionInParent
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.LocalLifecycleOwner
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.compose.ui.zIndex
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import kotlinx.coroutines.flow.drop
import kotlinx.coroutines.launch
import kotlin.math.max
import kotlin.math.min

/// One host: its terminal, the two strips (threads over the current
/// thread's tabs), the bars over the panes, the key bar with the soft
/// keyboard, and the tree, overview, settings and menus over it. Every
/// piece of chrome takes the terminal's own background, as on iOS.
@OptIn(ExperimentalFoundationApi::class)
@Composable
fun TerminalScreen(host: Host, store: HostStore, onBack: () -> Unit) {
    val context = LocalContext.current
    val model = remember(host.id) { TerminalModel(context, store, host) }
    val settings = model.settings
    val density = LocalDensity.current
    val scope = rememberCoroutineScope()
    val lang = AppLanguage.tag

    var showTree by remember { mutableStateOf(false) }
    var showSettings by remember { mutableStateOf(false) }
    val showLog = remember { mutableStateOf(false) }
    var editingHost by remember { mutableStateOf<Host?>(null) }
    var hostView by remember { mutableStateOf<TerminalHostView?>(null) }

    // The overview: the terminal shrinks into its thread's card and the
    // cards fade in around it, as the desktop's Live Overview zooms out.
    var overviewShown by remember { mutableStateOf(false) }
    var terminalBounds by remember { mutableStateOf(Rect.Zero) }
    var cardBounds by remember { mutableStateOf<Rect?>(null) }
    val zoom = remember { Animatable(0f) }
    // Once in the card the terminal fades into the card's own preview,
    // so the live card looks like every other one.
    val fade = remember { Animatable(0f) }
    // Once the terminal has faded, the live card shows its preview rows.
    var livePreview by remember { mutableStateOf(false) }
    val overviewOpen = overviewShown && zoom.value > 0f
    var zoomTarget by remember { mutableStateOf<Rect?>(null) }

    val keyboardUp = WindowInsets.ime.getBottom(density) > 0
    val twoLevel = settings.tabBarLevels == "two" && !(model.threads?.threads.isNullOrEmpty())
    val currentThread = model.threads?.current

    // The connection does not wait for the surface: the core attaches to
    // a pane when both have arrived, in whichever order they come.
    LaunchedEffect(model) { model.connect() }
    DisposableEffect(model) { onDispose { model.shutdown() } }

    // The open terminal follows the preferences as they change.
    LaunchedEffect(model) {
        snapshotFlow {
            listOf(
                settings.schemeName, settings.smoothScroll, settings.cursorStyle, settings.cursorBlink,
                settings.contrast, settings.resizeMode, settings.autoReconnect, settings.paneBars, settings.fontFamily,
            )
        }.drop(1).collect { model.settingsChanged() }
    }
    LaunchedEffect(showLog.value, settings.devMode) { model.wantsStats = showLog.value || settings.devMode }

    // Leaving the screen (Home, another app) with the setting off drops
    // the connection; coming back redials.
    val lifecycle = LocalLifecycleOwner.current.lifecycle
    DisposableEffect(lifecycle, model) {
        val observer = LifecycleEventObserver { _, event ->
            when (event) {
                Lifecycle.Event.ON_STOP -> model.enteredBackground()
                Lifecycle.Event.ON_START -> model.enteredForeground()
                else -> {}
            }
        }
        lifecycle.addObserver(observer)
        onDispose { lifecycle.removeObserver(observer) }
    }

    fun closeOverview() {
        if (!overviewShown) return
        scope.launch {
            livePreview = false
            fade.animateTo(0f, tween(150))
            zoom.animateTo(0f, spring(dampingRatio = 0.9f, stiffness = 400f))
            overviewShown = false
            zoomTarget = null
            cardBounds = null
            hostView?.input?.touchOverride = null
        }
    }

    fun openOverview() {
        if (overviewShown) return
        model.refreshViews()
        model.onHideKeyboard?.invoke()
        cardBounds = null
        zoomTarget = null
        overviewShown = true
        hostView?.input?.touchOverride = { closeOverview() }
        // Without a card of its own (no threads, or none current) the
        // terminal only fades.
        if (currentThread == null) {
            scope.launch { zoom.animateTo(1f, tween(250)) }
        }
    }

    // The card's thumbnail is laid out: the terminal goes there. Only the
    // first report counts; later ones must not restart the animation.
    fun cardLaidOut(frame: Rect?) {
        cardBounds = frame
        if (frame == null || !overviewShown || zoomTarget != null || terminalBounds.width <= 0f) return
        zoomTarget = frame
        scope.launch {
            zoom.animateTo(1f, spring(dampingRatio = 0.85f, stiffness = 300f))
            if (overviewShown) {
                fade.animateTo(1f, tween(180))
                livePreview = overviewShown
            }
        }
    }

    BackHandler {
        when {
            showSettings -> showSettings = false
            editingHost != null -> editingHost = null
            overviewShown -> closeOverview()
            else -> { model.disconnect(); onBack() }
        }
    }

    Box(
        Modifier
            .fillMaxSize()
            .background(lerp(model.background, Color(0xFF0F0F0F), zoom.value))
            .safeDrawingPadding()
    ) {
        if (overviewShown) {
            Box(Modifier.fillMaxSize().graphicsLayer { alpha = zoom.value }) {
                OverviewScreen(
                    model = model,
                    liveThread = currentThread?.id,
                    livePreview = livePreview,
                    onCardBounds = { cardLaidOut(it) },
                    onDismiss = { closeOverview() },
                )
            }
        }

        // Above the cards, so the shrunken terminal shows in its card. The
        // strips and bars leave while the overview is up, so the cards get
        // the taps; the terminal's own touches close it.
        Column(Modifier.fillMaxSize().zIndex(1f).imePadding()) {
            // The strips fade with the zoom and stop taking touches; their
            // room stays, so the terminal's own place does not move under
            // the zoom that was measured from it.
            Column(
                Modifier
                    .graphicsLayer { alpha = 1f - zoom.value }
                    .then(if (overviewOpen) Modifier.pointerInput(Unit) { awaitPointerEventScope { while (true) awaitPointerEvent().changes.forEach { it.consume() } } } else Modifier)
            ) {
                TopBar(
                    model = model,
                    twoLevel = twoLevel,
                    onBack = { model.disconnect(); onBack() },
                    onTree = { showTree = true },
                    onOverview = { openOverview() },
                    onEditHost = { editingHost = model.host },
                )
                if (twoLevel) TabSubstrip(model, onPinchOut = { openOverview() })
            }

            // The terminal, scaled to the card's width and moved into it as
            // the zoom goes from 0 to 1; the card keeps the bottom of it,
            // the newest rows, as the other cards' previews do.
            Box(
                Modifier
                    .fillMaxWidth()
                    .weight(1f)
                    .onGloballyPositioned { if (!overviewShown) terminalBounds = it.boundsInRoot() }
                    .graphicsLayer {
                        val t = zoom.value
                        val target = zoomTarget
                        transformOrigin = TransformOrigin(0f, 0f)
                        if (target != null && terminalBounds.width > 0) {
                            val s = target.width / terminalBounds.width
                            val scale = 1f + (s - 1f) * t
                            val top = (terminalBounds.height - target.height / s) * t
                            scaleX = scale
                            scaleY = scale
                            translationX = (target.left - terminalBounds.left) * t
                            translationY = (target.top - terminalBounds.top) * t - top * scale
                            // The card's own corners, once the terminal is in it.
                            val r = 13.dp.toPx() / scale * t
                            shape = RoundedCornerShape(bottomStart = r, bottomEnd = r)
                            alpha = 1f - fade.value
                        } else {
                            scaleX = 1f
                            scaleY = 1f
                            translationX = 0f
                            translationY = 0f
                            if (target == null && overviewShown) alpha = 1f - t
                        }
                        clip = true
                    }
                    .drawWithContent {
                        val target = zoomTarget
                        if (target != null && terminalBounds.width > 0) {
                            val s = target.width / terminalBounds.width
                            val top = (size.height - target.height / s) * zoom.value
                            clipRect(top = top) { this@drawWithContent.drawContent() }
                        } else {
                            drawContent()
                        }
                    }
                    .background(model.background)
            ) {
                AndroidView(
                    factory = { TerminalHostView(it, model).also { v -> hostView = v } },
                    modifier = Modifier.fillMaxSize(),
                )
                if (!overviewOpen) {
                    TerminalOverlays(model, showLog.value, onEditHost = { editingHost = model.host }, onBack = { model.disconnect(); onBack() })
                }
            }

            if (!overviewOpen) {
                StatusLine(model)
                if (keyboardUp) {
                    KeyBar(model = model, keyboardUp = keyboardUp, onOpenSettings = { showSettings = true })
                }
            }
        }

        if (showTree) {
            TreeSheet(model = model, onDismiss = { showTree = false })
        }
        if (showSettings) {
            Box(Modifier.fillMaxSize().zIndex(3f).background(MaterialTheme.colorScheme.background)) {
                SettingsScreen(model = model, showLog = showLog, onDone = { showSettings = false })
            }
        }
        editingHost?.let { h ->
            Box(Modifier.fillMaxSize().zIndex(3f).background(MaterialTheme.colorScheme.background)) {
                HostEditScreen(
                    store = store,
                    host = h,
                    onSave = { edited, secret, passphrase ->
                        if (secret.isNotEmpty() || store.secret(edited.id).isEmpty()) store.putSecret(edited.id, secret, passphrase)
                        store.upsert(edited)
                        model.host = edited
                        editingHost = null
                        model.connect()
                    },
                    onCancel = { editingHost = null },
                )
            }
        }
    }
}

// MARK: the bar

/// The desktop's two layers, as two rows: threads (the sidebar's layer)
/// in the bar between the back button and the menu, the current thread's
/// tabs in a strip under it. A server without threads shows its tabs in
/// the bar instead.
@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun TopBar(
    model: TerminalModel,
    twoLevel: Boolean,
    onBack: () -> Unit,
    onTree: () -> Unit,
    onOverview: () -> Unit,
    onEditHost: () -> Unit,
) {
    var connectionMenu by remember { mutableStateOf(false) }
    Row(
        Modifier.fillMaxWidth().height(48.dp).padding(start = 2.dp, end = 2.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        IconButton(onClick = onBack) {
            Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = tr("back"), tint = Color.White)
        }
        // The dot is the connection: a tap opens what can be done with it.
        Box {
            Box(
                Modifier
                    .size(28.dp)
                    .clip(CircleShape)
                    .clickable { connectionMenu = true }
                    .padding(10.dp)
                    .clip(CircleShape)
                    .background(statusColor(model))
            )
            ConnectionMenu(model, expanded = connectionMenu, onDismiss = { connectionMenu = false }, onEditHost = onEditHost)
        }
        val scroll = rememberScrollState()
        val positions = remember { mutableStateMapOf<String, Int>() }
        val currentKey = if (twoLevel) model.threads?.current?.id?.let { "thread:$it" } else model.tabs?.current?.tab?.let { "tab:$it" }
        ScrollToCurrent(scroll, positions, currentKey)
        Row(
            Modifier.weight(1f).horizontalScroll(scroll),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(4.dp),
        ) {
            if (twoLevel) {
                for (thread in model.threads?.threads ?: emptyList()) {
                    Box(Modifier.onGloballyPositioned { positions["thread:" + thread.id] = it.positionInParent().x.toInt() }) {
                        ThreadPill(model, thread, onTree)
                    }
                }
                PlusButton(onClick = { model.sideClick("new-thread") })
            } else {
                TabPills(model, size = 12.sp, height = 26.dp, positions = positions)
                NewTabButton(model)
            }
        }
        IconButton(onClick = onOverview) {
            Icon(Icons.Default.GridView, contentDescription = tr("overview"), tint = Color.White)
        }
    }
}

/// What the ⋯ menu used to hold about the host: reconnect or disconnect,
/// and the host's editor.
@Composable
private fun ConnectionMenu(model: TerminalModel, expanded: Boolean, onDismiss: () -> Unit, onEditHost: () -> Unit) {
    DropdownMenu(expanded = expanded, onDismissRequest = onDismiss) {
        val connected = model.isConnected
        Text(
            model.host.display,
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            modifier = Modifier.padding(horizontal = 16.dp, vertical = 6.dp),
        )
        DropdownMenuItem(
            text = { Text(if (connected) tr("disconnect") else tr("reconnect")) },
            onClick = { if (connected) model.disconnect() else model.connect(); onDismiss() },
        )
        if (model.host.id != Host.PROBE_ID) {
            DropdownMenuItem(text = { Text(tr("edithost")) }, onClick = { onDismiss(); onEditHost() })
        }
    }
}

/// "+" opens a tab; held, it offers the splits too.
@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun NewTabButton(model: TerminalModel) {
    var menu by remember { mutableStateOf(false) }
    Box {
        PlusButton(onClick = { model.chromeClick("new-tab") }, onLongClick = { menu = true })
        DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
            DropdownMenuItem(text = { Text(tr("newtab")) }, onClick = { model.chromeClick("new-tab"); menu = false })
            DropdownMenuItem(text = { Text(tr("split.right")) }, onClick = { model.chromeClick("split-right"); menu = false })
            DropdownMenuItem(text = { Text(tr("split.below")) }, onClick = { model.chromeClick("split-below"); menu = false })
        }
    }
}

@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun ThreadPill(model: TerminalModel, thread: ThreadView, onTree: () -> Unit) {
    var menu by remember { mutableStateOf(false) }
    var confirmDelete by remember { mutableStateOf(false) }
    if (confirmDelete) {
        // Deleting ends every program in the thread: it is asked first.
        AlertDialog(
            onDismissRequest = { confirmDelete = false },
            title = { Text(tr("thread.delete.title", thread.name)) },
            text = { Text(tr("thread.delete.body")) },
            confirmButton = {
                TextButton(onClick = { confirmDelete = false; model.sideClick("delete", thread.id) }) {
                    Text(tr("delete"), color = MaterialTheme.colorScheme.error)
                }
            },
            dismissButton = { TextButton(onClick = { confirmDelete = false }) { Text(tr("cancel")) } },
        )
    }
    Box {
        Row(
            Modifier
                .clip(RoundedCornerShape(50))
                .background(Color.White.copy(alpha = if (thread.current) 0.18f else 0.06f))
                .combinedClickable(
                    // The one on show opens the tree; another is shown.
                    onClick = { if (thread.current) onTree() else model.sideClick("thread", thread.id) },
                    onLongClick = { menu = true },
                )
                .widthIn(min = 64.dp)
                .height(26.dp)
                .padding(start = 10.dp, end = if (thread.current) 4.dp else 10.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(5.dp),
        ) {
            Box(Modifier.size(6.dp).clip(CircleShape).background(threadColor(thread.status, thread.live)))
            Text(
                thread.name,
                color = Color.White,
                fontSize = 12.sp,
                fontWeight = if (thread.current) FontWeight.SemiBold else FontWeight.Normal,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
            if (thread.current) CloseX { confirmDelete = true }
        }
        AppContextMenu(model, "thread", thread.id, expanded = menu, onDismiss = { menu = false })
    }
}

/// The strip follows the current pill: it scrolls so the pill sits a
/// third of the way in, as the iOS strips do.
@Composable
private fun ScrollToCurrent(scroll: androidx.compose.foundation.ScrollState, positions: Map<String, Int>, key: String?) {
    val x = key?.let { positions[it] }
    LaunchedEffect(key, x) {
        if (x != null) scroll.animateScrollTo((x - scroll.viewportSize / 3).coerceAtLeast(0))
    }
}

@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun TabPills(
    model: TerminalModel,
    size: androidx.compose.ui.unit.TextUnit,
    height: androidx.compose.ui.unit.Dp,
    positions: MutableMap<String, Int>,
) {
    for (tab in model.tabs?.tabs ?: emptyList()) {
        var menu by remember(tab.tab) { mutableStateOf(false) }
        Box(Modifier.onGloballyPositioned { positions["tab:" + tab.tab] = it.positionInParent().x.toInt() }) {
            Row(
                Modifier
                    .clip(RoundedCornerShape(50))
                    .background(Color.White.copy(alpha = if (tab.current) 0.18f else 0.06f))
                    .combinedClickable(
                        onClick = { model.chromeClick("pane", pane = tab.target) },
                        onLongClick = { menu = true },
                    )
                    .widthIn(min = 76.dp, max = 160.dp)
                    .heightIn(min = height)
                    .padding(start = 10.dp, end = if (tab.current) 4.dp else 10.dp, top = 4.dp, bottom = 4.dp),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(4.dp),
            ) {
                Text(
                    text = tab.label.ifEmpty { tr("tab.num", tab.tab) },
                    color = if (tab.current) Color.White else Color.White.copy(alpha = 0.62f),
                    fontSize = size,
                    fontWeight = if (tab.current) FontWeight.SemiBold else FontWeight.Normal,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                    textAlign = TextAlign.Center,
                    modifier = Modifier.weight(1f, fill = false),
                )
                if (tab.current) CloseX { model.chromeClick("close-tab", tab = tab.tab) }
            }
            AppContextMenu(model, "tab", tab.tab.toString(), expanded = menu, onDismiss = { menu = false })
        }
    }
}

/// The × at the end of the current pill: what the desktop's tabs carry.
@Composable
private fun CloseX(onClick: () -> Unit) {
    Icon(
        Icons.Default.Close,
        contentDescription = tr("closetab"),
        tint = Color.White.copy(alpha = 0.7f),
        modifier = Modifier
            .size(18.dp)
            .clip(CircleShape)
            .clickable(onClick = onClick)
            .padding(3.dp),
    )
}

@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun PlusButton(onClick: () -> Unit, onLongClick: (() -> Unit)? = null) {
    Icon(
        Icons.Default.Add,
        contentDescription = tr("newtab"),
        tint = Color.White.copy(alpha = 0.62f),
        modifier = Modifier
            .size(28.dp)
            .clip(CircleShape)
            .combinedClickable(onClick = onClick, onLongClick = onLongClick)
            .padding(5.dp),
    )
}

/// The current thread's tabs, under the bar. Pinching the strip in zooms
/// out to the overview.
@Composable
private fun TabSubstrip(model: TerminalModel, onPinchOut: () -> Unit) {
    var pinch by remember { mutableStateOf(1f) }
    val scroll = rememberScrollState()
    val positions = remember { mutableStateMapOf<String, Int>() }
    ScrollToCurrent(scroll, positions, model.tabs?.current?.tab?.let { "tab:$it" })
    Row(
        Modifier
            .fillMaxWidth()
            .height(32.dp)
            .background(model.background)
            .pointerInput(Unit) {
                detectTransformGestures { _, _, zoom, _ ->
                    pinch *= zoom
                    if (pinch < 0.8f) {
                        pinch = 1f
                        onPinchOut()
                    }
                }
            }
            .padding(horizontal = 8.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Row(
            Modifier.weight(1f).horizontalScroll(scroll),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(4.dp),
        ) {
            TabPills(model, size = 11.5.sp, height = 22.dp, positions = positions)
        }
        NewTabButton(model)
    }
}

private fun statusColor(model: TerminalModel): Color = when {
    model.reconnecting -> Color(0xFFFF9F0A)
    model.isConnected -> Color(0xFF34C759)
    model.connection.startsWith("connecting") || model.connection.contains("reconnect") -> Color(0xFFFF9F0A)
    else -> Color(0xFFFF453A)
}

// MARK: over the terminal

/// The bars over split panes, the scrollbar, the stats, the composition,
/// the take-over card, the log, the selection's copy chip, and the card
/// while there is no terminal to show.
@Composable
private fun BoxScope.TerminalOverlays(model: TerminalModel, showLog: Boolean, onEditHost: () -> Unit, onBack: () -> Unit) {
    val settings = model.settings
    if (model.navs.size > 1 && settings.paneBars) {
        NavBars(model)
    }
    if (settings.scrollbar && model.scrollMax > 0 && model.scrollShown) {
        Scrollbar(model)
    }
    if (settings.devMode) {
        Text(
            model.stats,
            fontSize = 8.sp,
            fontFamily = FontFamily.Monospace,
            color = Color.White.copy(alpha = 0.7f),
            modifier = Modifier.align(Alignment.BottomStart).background(Color.Black.copy(alpha = 0.5f)).padding(4.dp),
        )
    }
    model.composing?.let { composing ->
        Text(
            composing,
            fontSize = 14.sp,
            fontFamily = FontFamily.Monospace,
            color = Color.Black,
            modifier = Modifier
                .padding(8.dp)
                .clip(RoundedCornerShape(4.dp))
                .background(Color(0xE6FFEB3B))
                .padding(4.dp),
        )
    }
    model.selectionMenuAt?.let { (x, y) ->
        // The chip sits over the head cell, a row above it when it fits.
        val top = (y - 40).coerceAtLeast(0.0)
        Text(
            if (model.copied) tr("copied") else tr("copy"),
            color = Color.White,
            fontSize = 13.sp,
            modifier = Modifier
                .offset(x = x.dp, y = top.dp)
                .clip(RoundedCornerShape(8.dp))
                .background(Color(0xFF2C2C2E))
                .clickable { model.copySelection() }
                .padding(horizontal = 12.dp, vertical = 7.dp),
        )
    }
    model.status?.card?.let { card ->
        Column(
            Modifier
                .align(Alignment.Center)
                .widthIn(max = 320.dp)
                .clip(RoundedCornerShape(12.dp))
                .background(Color(0xF0202124))
                .padding(16.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            Text(card.title, color = Color.White, fontWeight = FontWeight.SemiBold)
            Text(card.hint, color = Color.White.copy(alpha = 0.7f), fontSize = 12.sp, textAlign = TextAlign.Center)
            Button(onClick = { model.takeOver() }, enabled = card.state != "taking") { Text(card.action) }
        }
    }
    if (showLog) {
        Text(
            model.logText + "\n" + model.stats,
            fontSize = 9.sp,
            fontFamily = FontFamily.Monospace,
            color = Color.White,
            modifier = Modifier
                .align(Alignment.BottomStart)
                .fillMaxWidth()
                .heightIn(max = 220.dp)
                .background(Color.Black.copy(alpha = 0.8f))
                .verticalScroll(rememberScrollState())
                .padding(6.dp),
        )
    }
    val phase = ConnectionPhase.of(model.connection)
    if (model.reconnecting && !settings.autoReconnect && phase == null) {
        // Asked not to redial: the card offers it instead.
        ConnectionCard(ConnectionPhase.Disconnected, model.host.display, canEdit = model.host.id != Host.PROBE_ID) { action ->
            when (action) {
                CardAction.Back -> onBack()
                else -> model.connect()
            }
        }
    }
    if (phase != null) {
        ConnectionCard(phase, model.host.display, canEdit = model.host.id != Host.PROBE_ID) { action ->
            when (action) {
                CardAction.Retry -> model.connect()
                CardAction.ForgetKeyAndRetry -> {
                    model.store.forgetHostKey(model.host.id)
                    model.host = model.host.copy(knownHost = null)
                    model.connect()
                }
                CardAction.Edit -> onEditHost()
                CardAction.Back -> onBack()
            }
        }
    }
}

/// The App leaves rows above each pane for its bar; these draw it where
/// the App says (dp, from the terminal's origin). Dragging a bar drags
/// the divider it sits under (or beside): the press lands just outside
/// the bar, and the App does the rest.
@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun BoxScope.NavBars(model: TerminalModel) {
    val density = LocalDensity.current
    for (nav in model.navs) {
        val r = nav.rect
        var menu by remember(r.pane) { mutableStateOf(false) }
        Box(
            Modifier
                .offset(x = r.left.dp, y = r.top.dp)
                .width(r.width.dp)
                .height(r.height.dp)
                .background(model.background)
                .background(Color.White.copy(alpha = if (nav.focused) 0.1f else 0.04f))
                .pointerInput(nav.rect) {
                    var origin: Offset? = null
                    detectDragGestures(
                        onDragStart = { start ->
                            val s = Offset(start.x / density.density, start.y / density.density)
                            origin = when {
                                r.top > 0 -> Offset((r.left + s.x).toFloat(), (r.top - 2).toFloat())
                                r.left > 0 -> Offset((r.left - 2).toFloat(), (r.top + s.y).toFloat())
                                else -> null
                            }
                            origin?.let { model.pointer("down", it.x.toDouble(), it.y.toDouble()) }
                        },
                        onDrag = { change, _ ->
                            val o = origin ?: return@detectDragGestures
                            val p = change.position
                            model.pointer("move", (o.x + p.x / density.density).toDouble(), (o.y + p.y / density.density).toDouble())
                        },
                        onDragEnd = {
                            origin?.let { model.pointer("up", it.x.toDouble(), it.y.toDouble()) }
                            origin = null
                        },
                        onDragCancel = { origin = null },
                    )
                }
                .combinedClickable(onClick = {}, onLongClick = { menu = true })
                .padding(horizontal = 6.dp),
        ) {
            Row(Modifier.fillMaxSize(), verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                val tint = if (nav.focused) Color.White else Color(0xFF9E9E9E)
                for (member in nav.members) {
                    Row(
                        Modifier
                            .clip(RoundedCornerShape(50))
                            .background(if (member.current) Color.White.copy(alpha = if (nav.focused) 0.22f else 0.12f) else Color.Transparent)
                            .clickable { model.chromeClick("pane", pane = member.pane) }
                            .padding(horizontal = 6.dp, vertical = 2.dp),
                        verticalAlignment = Alignment.CenterVertically,
                        horizontalArrangement = Arrangement.spacedBy(4.dp),
                    ) {
                        if (member.busy) CircularProgressIndicator(Modifier.size(9.dp), strokeWidth = 1.5.dp, color = tint)
                        Text(
                            member.title.ifEmpty { "shell" },
                            color = tint,
                            fontSize = 11.sp,
                            fontWeight = if (member.current) FontWeight.SemiBold else FontWeight.Normal,
                            maxLines = 1,
                            overflow = TextOverflow.Ellipsis,
                        )
                    }
                }
                Spacer(Modifier.weight(1f))
                Icon(Icons.Default.Add, contentDescription = null, tint = tint, modifier = Modifier.size(16.dp).clickable { model.chromeClick("new-pane", pane = r.pane) })
                Icon(Icons.Default.Close, contentDescription = null, tint = tint, modifier = Modifier.size(16.dp).clickable { model.chromeClick("close-pane", pane = r.pane) })
            }
            AppContextMenu(model, "pane", r.pane.toString(), expanded = menu, onDismiss = { menu = false })
        }
    }
}

/// A thin bar at the right: where the view is in the scrollback.
@Composable
private fun BoxScope.Scrollbar(model: TerminalModel) {
    var height by remember { mutableStateOf(0f) }
    val density = LocalDensity.current
    Box(
        Modifier
            .fillMaxSize()
            .onGloballyPositioned { height = it.size.height / density.density }
    ) {
        val rowsVisible = height / max(model.cellHeight, 1.0).toFloat()
        val total = model.scrollMax + rowsVisible
        val thumb = max(height * rowsVisible / max(total, 1f), 24f)
        val track = height - thumb
        val y = track * (1f - model.scrollAbove.toFloat() / max(model.scrollMax, 1).toFloat())
        Box(
            Modifier
                .align(Alignment.TopEnd)
                .offset(x = (-3).dp, y = y.dp)
                .size(3.dp, thumb.dp)
                .clip(RoundedCornerShape(2.dp))
                .background(Color.White.copy(alpha = 0.35f))
        )
    }
}

/// The App's remark, "copied", or the connection's own line while it is
/// not yet a pane.
@Composable
private fun StatusLine(model: TerminalModel) {
    val text = if (model.copied) tr("copied") else (model.toastText ?: "")
    val c = model.connection
    val showing = text.isNotEmpty() || c.startsWith("connecting") || c.startsWith("disconnected") || c.startsWith("failed") || c.contains("reconnect")
    if (!showing) return
    Text(
        text.ifEmpty { c },
        color = Color.White,
        fontSize = 11.sp,
        fontFamily = FontFamily.Monospace,
        maxLines = 2,
        modifier = Modifier.fillMaxWidth().background(Color(0xFF333333)).padding(horizontal = 8.dp, vertical = 4.dp),
    )
}

// MARK: the connection card

/// Where a connection is, read off the core's status line.
sealed class ConnectionPhase {
    data class Connecting(val target: String) : ConnectionPhase()
    object Attaching : ConnectionPhase()
    object Reconnecting : ConnectionPhase()
    data class Failed(val reason: String) : ConnectionPhase()
    object Disconnected : ConnectionPhase()

    companion object {
        fun of(status: String): ConnectionPhase? = when {
            status.startsWith("pane ") -> null
            status.startsWith("connecting") -> Connecting(status.removePrefix("connecting to ").removePrefix("connecting"))
            status.startsWith("connected") -> Attaching
            status.contains("reconnect") -> Reconnecting
            status.startsWith("failed: ") -> Failed(status.removePrefix("failed: "))
            status.startsWith("attach failed: ") -> Failed(status.removePrefix("attach failed: "))
            status.startsWith("disconnected: ") -> Failed(status.removePrefix("disconnected: "))
            status.startsWith("disconnected") -> Disconnected
            // "idle", or nothing yet: the connection is about to start.
            else -> Connecting("")
        }
    }
}

enum class CardAction { Retry, ForgetKeyAndRetry, Edit, Back }

/// The card over the terminal while there is no terminal to show: the
/// steps of a connection, or what stopped it and what to do about it.
@Composable
private fun BoxScope.ConnectionCard(phase: ConnectionPhase, hostName: String, canEdit: Boolean, act: (CardAction) -> Unit) {
    Box(Modifier.fillMaxSize().background(Color.Black.copy(alpha = 0.4f)), contentAlignment = Alignment.Center) {
        Column(
            Modifier
                .widthIn(max = 340.dp)
                .clip(RoundedCornerShape(16.dp))
                .background(Color(0xF5202124))
                .padding(20.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.spacedBy(14.dp),
        ) {
            when (phase) {
                is ConnectionPhase.Connecting -> {
                    CircularProgressIndicator()
                    Text(tr("card.connecting", hostName), color = Color.White, fontWeight = FontWeight.SemiBold)
                    if (phase.target.isNotEmpty()) {
                        Text(phase.target, color = Color.White.copy(alpha = 0.6f), fontSize = 12.sp, fontFamily = FontFamily.Monospace)
                    }
                    Steps(done = 0)
                }
                ConnectionPhase.Attaching -> {
                    CircularProgressIndicator()
                    Text(tr("card.attaching", hostName), color = Color.White, fontWeight = FontWeight.SemiBold)
                    Steps(done = 1)
                }
                ConnectionPhase.Reconnecting -> {
                    CircularProgressIndicator()
                    Text(tr("card.reconnecting", hostName), color = Color.White, fontWeight = FontWeight.SemiBold)
                }
                ConnectionPhase.Disconnected -> {
                    Icon(Icons.Outlined.PowerOff, contentDescription = null, tint = Color.White.copy(alpha = 0.6f), modifier = Modifier.size(34.dp))
                    Text(tr("conn.disconnected"), color = Color.White, fontWeight = FontWeight.SemiBold)
                    Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                        OutlinedButton(onClick = { act(CardAction.Back) }) { Text(tr("back")) }
                        Button(onClick = { act(CardAction.Retry) }) { Text(tr("reconnect")) }
                    }
                }
                is ConnectionPhase.Failed -> {
                    Icon(Icons.Default.Warning, contentDescription = null, tint = Color(0xFFFF9F0A), modifier = Modifier.size(34.dp))
                    Text(tr("card.failed", hostName), color = Color.White, fontWeight = FontWeight.SemiBold, textAlign = TextAlign.Center)
                    Text(
                        phase.reason,
                        color = Color.White.copy(alpha = 0.6f),
                        fontSize = 12.sp,
                        fontFamily = FontFamily.Monospace,
                        textAlign = TextAlign.Center,
                        maxLines = 6,
                        overflow = TextOverflow.Ellipsis,
                    )
                    hintFor(phase.reason)?.let { Text(it, color = Color.White, fontSize = 13.sp, textAlign = TextAlign.Center) }
                    Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                        OutlinedButton(onClick = { act(CardAction.Back) }) { Text(tr("back")) }
                        if (canEdit) OutlinedButton(onClick = { act(CardAction.Edit) }) { Text(tr("edithost")) }
                        if (phase.reason.contains("key changed") && canEdit) {
                            Button(onClick = { act(CardAction.ForgetKeyAndRetry) }) { Text(tr("forgetretry")) }
                        } else {
                            Button(onClick = { act(CardAction.Retry) }) { Text(tr("retry")) }
                        }
                    }
                }
            }
        }
    }
}

@Composable
private fun Steps(done: Int) {
    Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
        Step(tr("conn.step1"), state = if (done > 0) 2 else 1)
        Step(tr("conn.step2"), state = if (done > 1) 2 else if (done == 1) 1 else 0)
    }
}

@Composable
private fun Step(label: String, state: Int) {
    Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
        Icon(
            if (state == 2) Icons.Default.CheckCircle else Icons.Outlined.Circle,
            contentDescription = null,
            tint = if (state == 2) Color(0xFF34C759) else Color.White.copy(alpha = 0.5f),
            modifier = Modifier.size(16.dp),
        )
        Text(label, color = if (state == 0) Color.White.copy(alpha = 0.5f) else Color.White, fontSize = 13.sp)
    }
}

/// What the reason usually means, in plain words.
private fun hintFor(reason: String): String? {
    val r = reason.lowercase()
    return when {
        r.contains("refused the login") || r.contains("auth") -> tr("hint.auth")
        r.contains("key changed") -> tr("hint.keychanged")
        r.contains("timed out") || r.contains("connection refused") || r.contains("unreachable") || r.contains("no route") -> tr("hint.unreachable")
        r.contains("not found") || r.contains("exit 127") || r.contains("no such file") -> tr("hint.notfound")
        r.contains("codec") || r.contains("version") -> tr("hint.version")
        else -> null
    }
}
