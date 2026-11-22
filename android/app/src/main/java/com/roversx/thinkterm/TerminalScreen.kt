package com.roversx.thinkterm

import androidx.activity.compose.BackHandler
import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.FastOutSlowInEasing
import androidx.compose.animation.slideOutHorizontally
import androidx.compose.animation.slideInHorizontally
import androidx.compose.animation.slideOutVertically
import androidx.compose.animation.slideInVertically
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.EnterTransition
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.core.spring
import androidx.compose.animation.core.tween
import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.gestures.detectDragGestures
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxScope
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.ime
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.requiredSize
import androidx.compose.foundation.layout.safeDrawingPadding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.safeDrawingPadding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Add
import androidx.compose.material.icons.filled.CheckCircle
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.Warning
import androidx.compose.material.icons.outlined.PowerOff
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.key
import androidx.compose.runtime.mutableIntStateOf
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
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Shadow
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.TransformOrigin
import androidx.compose.ui.graphics.drawscope.clipRect
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.graphics.lerp
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.layout.boundsInRoot
import androidx.compose.ui.layout.layout
import androidx.compose.ui.layout.onGloballyPositioned
import androidx.compose.ui.layout.positionInParent
import androidx.compose.ui.layout.positionInRoot
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.foundation.text.BasicText
import androidx.compose.foundation.text.TextAutoSize
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.compose.ui.zIndex
import kotlin.math.max
import kotlin.math.min
import com.kyant.backdrop.backdrops.layerBackdrop
import com.kyant.backdrop.backdrops.rememberLayerBackdrop
import kotlinx.coroutines.flow.drop
import kotlinx.coroutines.launch
import kotlinx.coroutines.delay

/// One host: its terminal, the two strips (threads over the current
/// thread's tabs), the bars over the panes, the key bar with the soft
/// keyboard, and the tree, overview, settings and menus over it. Every
/// piece of chrome takes the terminal's own background, as on iOS.
@OptIn(ExperimentalFoundationApi::class)
@Composable
fun TerminalScreen(host: Host, store: HostStore, onBack: () -> Unit) {
    val context = LocalContext.current
    // The connection is the host's, not the screen's: it lives in
    // `Sessions` and is picked up again on the way back.
    val model = remember(host.id) { Sessions.get(context, store, host) }
    val settings = model.settings
    val density = LocalDensity.current
    val scope = rememberCoroutineScope()
    val lang = AppLanguage.tag

    var showTree by remember { mutableStateOf(false) }
    /// The right sidebar is out from under the screen (`RightPanel`).
    var showRight by remember { mutableStateOf(false) }
    // How far the screen has slid off each sidebar, 0 to 1: they lie under
    // it, as on iOS, the right one the left one's mirror.
    val leftSlide = remember { Animatable(0f) }
    val rightSlide = remember { Animatable(0f) }
    LaunchedEffect(showTree) { leftSlide.animateTo(if (showTree) 1f else 0f, spring(dampingRatio = 0.86f, stiffness = 420f)) }
    LaunchedEffect(showRight) { rightSlide.animateTo(if (showRight) 1f else 0f, spring(dampingRatio = 0.86f, stiffness = 420f)) }
    var showSettings by remember { mutableStateOf(false) }
    val showLog = remember { mutableStateOf(false) }
    var editingHost by remember { mutableStateOf<Host?>(null) }
    var hostView by remember { mutableStateOf<TerminalHostView?>(null) }
    // What the glass over the terminal shows through: the terminal alone,
    // not the overlays, the glass among them.
    val glass = rememberLayerBackdrop()
    // What the glass under the terminal shows through: the page's ground.
    val pageGlass = rememberLayerBackdrop()
    /// The split of the tab on show is up as a map (`PaneMap`).
    var showPanes by remember { mutableStateOf(false) }
    // No room for a split's panes side by side: one at a time.
    val configuration = LocalConfiguration.current
    val wide = configuration.screenWidthDp >= 700
    // A keyboard of the device's own, open (desktop mode, a tablet's case).
    val hardKeyboard = configuration.keyboard == android.content.res.Configuration.KEYBOARD_QWERTY &&
        configuration.hardKeyboardHidden == android.content.res.Configuration.HARDKEYBOARDHIDDEN_NO
    LaunchedEffect(model, wide) {
        model.setOnePane(!wide)
        // Wide, the sidebar is a column: the drawer has no place.
        if (wide) showTree = false
    }
    val pinned = wide && settings.sidebarPinned
    // The terminal's scheme follows the phone's side when none is picked.
    val systemDark = isSystemInDarkTheme()
    LaunchedEffect(model, systemDark) { model.appearanceMayHaveChanged() }

    // The overview: the terminal shrinks into its thread's card and the
    // cards fade in around it, as the desktop's Live Overview zooms out.
    var overviewShown by remember { mutableStateOf(false) }
    var terminalBounds by remember { mutableStateOf(Rect.Zero) }
    // Where the screen's padded box starts, in root pixels: the frames
    // the cards report are root pixels too.
    var boxOrigin by remember { mutableStateOf(Offset.Zero) }
    var cardBounds by remember { mutableStateOf<CardFrames?>(null) }
    val zoom = remember { Animatable(0f) }
    // Once in the card the terminal fades into the card's own preview,
    // so the live card looks like every other one.
    val fade = remember { Animatable(0f) }
    // Once the terminal has faded, the live card shows its preview rows.
    var livePreview by remember { mutableStateOf(false) }
    val overviewOpen = overviewShown && zoom.value > 0f
    var zoomTarget by remember { mutableStateOf<CardFrames?>(null) }
    /// The thread whose card the terminal is moving into or out of.
    var heroThread by remember { mutableStateOf<ThreadView?>(null) }

    val keyboardUp = WindowInsets.ime.getBottom(density) > 0
    val currentThread = model.threads?.current

    // The connection does not wait for the surface: the core attaches to
    // a pane when both have arrived, in whichever order they come.
    // One lost and not redialing by itself (auto-reconnect off) is dialled
    // again: coming to it -- the host page's Connect, a thread of it -- is
    // the tap that asks for it.
    LaunchedEffect(model) {
        model.enterScreen()
        if (!model.isConnected || (model.reconnecting && !model.autoReconnect)) model.connect()
    }
    // Leaving takes the picture, not the connection: the frame loop
    // rests, the surface goes with the view, the socket stays.
    DisposableEffect(model) { onDispose { model.leaveScreen() } }

    // The open terminal follows the preferences as they change.
    LaunchedEffect(model) {
        snapshotFlow { model.followedSettings() }.drop(1).collect { model.settingsChanged() }
    }
    LaunchedEffect(showLog.value, settings.devMode) { model.wantsStats = showLog.value || settings.devMode }


    /// Back to the terminal. A tapped card hands over its thread and its
    /// frames: the terminal grows out of that card, not the one it came
    /// from.
    fun closeOverview(picked: ThreadView? = null, frames: CardFrames? = null) {
        if (!overviewShown) return
        if (picked != null && frames != null) {
            heroThread = picked
            zoomTarget = frames
        }
        scope.launch {
            livePreview = false
            hostView?.showPicture(true)
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
        heroThread = currentThread
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
    fun cardLaidOut(frame: CardFrames?) {
        cardBounds = frame
        if (frame == null || !overviewShown || zoomTarget != null || terminalBounds.width <= 0f) return
        zoomTarget = frame
        scope.launch {
            zoom.animateTo(1f, spring(dampingRatio = 0.85f, stiffness = 300f))
            if (overviewShown) {
                fade.animateTo(1f, tween(180))
                // A faded layer is not enough: the texture repaints itself on
                // every new frame (the cursor's blink) and can show through
                // for one; hidden outright, it cannot.
                hostView?.showPicture(false)
                livePreview = overviewShown
            }
        }
    }

    BackHandler {
        when {
            // What is over everything goes first: the sidebar waits under
            // the settings and the host's editor.
            showSettings -> showSettings = false
            editingHost != null -> editingHost = null
            showTree -> showTree = false
            showRight -> showRight = false
            showPanes -> showPanes = false
            overviewShown -> closeOverview()
            // The session stays, as the bar's back button leaves it: the
            // threads list shows its threads.
            else -> onBack()
        }
    }

    // The pages over the terminal (settings, the host's editor) are the
    // app's, in the system's lightness; the rest takes the terminal's.
    val appScheme = MaterialTheme.colorScheme
    val pageOpen = showSettings || editingHost != null
    SystemBars(
        dark = when {
            pageOpen -> isSystemInDarkTheme()
            overviewShown -> true
            else -> Chrome.of(model.background).dark
        },
    )

    TerminalChrome(model.background) {
        val chrome = LocalChrome.current
        BoxWithConstraints(Modifier.fillMaxSize().background(listBackdrop(chrome.dark))) {
            val drawerWidth = minOf(320.dp, maxWidth * 0.84f)
            val drawerPx = with(density) { drawerWidth.toPx() }
            if (showTree || leftSlide.value > 0f) {
                Box(Modifier.width(drawerWidth).fillMaxHeight().align(Alignment.TopStart)) {
                    SidebarPanel(
                        model = model,
                        done = { showTree = false },
                        // Over the sidebar, which waits under them, as iOS's
                        // stack pushes a page over the drawer it came from.
                        onEditHost = { editingHost = model.host },
                        onSettings = { showSettings = true },
                        onOverview = { showTree = false; if (overviewShown) closeOverview() else openOverview() },
                        // The whole page goes, the sidebar with it: one move.
                        onLeave = { onBack() },
                        overviewOn = overviewShown,
                    )
                }
            }
            if (showRight || rightSlide.value > 0f) {
                Box(Modifier.width(drawerWidth).fillMaxHeight().align(Alignment.TopEnd)) {
                    RightPanel(model) { showRight = false }
                }
            }
            val moved = leftSlide.value > 0f || rightSlide.value > 0f
            // The screen, the whole height of the display: it slides off a
            // sidebar with its corners rounded and a shadow under it.
            Box(
                Modifier
                    .fillMaxSize()
                    .graphicsLayer {
                        translationX = (leftSlide.value - rightSlide.value) * drawerPx
                        if (leftSlide.value > 0f || rightSlide.value > 0f) {
                            shape = RoundedCornerShape(28.dp)
                            clip = true
                            shadowElevation = 24.dp.toPx()
                        }
                    }
            ) {
                // On a wide screen the sidebar stays beside the terminal, a
                // column the bar's button shows and hides; on a narrow one
                // it is the drawer the screen slides off.
                Row(Modifier.fillMaxSize()) {
                    if (pinned) {
                        Box(Modifier.width(SIDEBAR_COLUMN).fillMaxHeight()) {
                            SidebarPanel(
                                model = model,
                                done = {},
                                onEditHost = { editingHost = model.host },
                                onSettings = { showSettings = true },
                                onOverview = { if (overviewShown) closeOverview() else openOverview() },
                                onLeave = { onBack() },
                                overviewOn = overviewShown,
                            )
                        }
                        Box(Modifier.width(1.dp).fillMaxHeight().background(ink.copy(alpha = 0.08f)))
                    }
                    Box(
                        Modifier
                            .weight(1f)
                            .fillMaxHeight()
                            .background(lerp(model.background, Color(0xFF0F0F0F), zoom.value))
                            .safeDrawingPadding()
                            .onGloballyPositioned { boxOrigin = it.positionInRoot() }
                    ) {
                        Box(
                            Modifier
                                .matchParentSize()
                                .layerBackdrop(pageGlass)
                                .background(lerp(model.background, Color(0xFF0F0F0F), zoom.value))
                        )
                        if (overviewShown) {
                            // Under the bar, which stays: the threads and the way back.
                            Box(Modifier.fillMaxSize().padding(top = 48.dp).graphicsLayer { alpha = zoom.value }) {
                                OverviewScreen(
                                    model = model,
                                    liveThread = currentThread?.id,
                                    livePreview = livePreview,
                                    onCardBounds = { cardLaidOut(it) },
                                    onDismiss = { picked, frames -> closeOverview(picked, frames) },
                                )
                            }
                        }

                        // Above the cards, so the shrunken terminal shows in its card. The
                        // strips and bars leave while the overview is up, so the cards get
                        // the taps; the terminal's own touches close it.
                        Column(Modifier.fillMaxSize().zIndex(1f).imePadding()) {
                            // The bar stays through the overview, as the iOS navigation
                            // bar does: there it carries the overview's name and the
                            // way back.
                            TopBar(
                                model = model,
                                glass = pageGlass,
                                overview = overviewShown,
                                // The keyboard goes first, or the panel sits on it.
                                // A column on a wide screen, shown or hidden; the
                            // drawer on a narrow one.
                            onTree = {
                                if (wide) {
                                    settings.sidebarPinned = !settings.sidebarPinned
                                } else {
                                    model.onHideKeyboard?.invoke()
                                    showTree = true
                                }
                            },
                                onRight = { model.onHideKeyboard?.invoke(); showRight = true },
                                onCloseOverview = { closeOverview() },
                            )

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
                                            val thumb = target.thumb
                                            val s = thumb.width / terminalBounds.width
                                            val scale = 1f + (s - 1f) * t
                                            val top = (terminalBounds.height - thumb.height / s) * t
                                            scaleX = scale
                                            scaleY = scale
                                            translationX = (thumb.left - terminalBounds.left) * t
                                            translationY = (thumb.top - terminalBounds.top) * t - top * scale
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
                                            val s = target.thumb.width / terminalBounds.width
                                            val top = (size.height - target.thumb.height / s) * zoom.value
                                            clipRect(top = top) { this@drawWithContent.drawContent() }
                                        } else {
                                            drawContent()
                                        }
                                    }
                                    .background(model.background)
                            ) {
                                AndroidView(
                                    factory = { TerminalHostView(it, model).also { v -> hostView = v } },
                                    modifier = Modifier.fillMaxSize().layerBackdrop(glass),
                                )
                                if (!overviewOpen) {
                                    CompositionLocalProvider(LocalGlass provides glass) {
                                        TerminalOverlays(model, showLog.value)
                                        // Over the terminal's top: in the layout it
                                        // would resize the terminal as it comes and goes.
                                        StatusCapsule(model, Modifier.align(Alignment.TopCenter).padding(top = 8.dp, start = 16.dp, end = 16.dp))
                                    }
                                }
                            }

                            if (!overviewOpen) {
                                // Under the terminal whether the keyboard is up or not,
                                // so the terminal is sized once as it comes and goes:
                                // the tab's panes, then the keys.
                                // Out of sight while the connection page covers the
                                // terminal, but still in the layout: the terminal keeps
                                // its size, and the pane is not resized twice a dial.
                                val covered = connectionCovers(model)
                                // The first moments of a dial back from the
                                // background: dimmed, and not taking a touch,
                                // the connection not being live yet.
                                val held = model.cardHeld &&
                                    ConnectionPhase.of(model.connection).let { it != null && it !is ConnectionPhase.Failed }
                                val keysAlpha by androidx.compose.animation.core.animateFloatAsState(
                                    if (covered) 0f else if (held) 0.4f else 1f, tween(200), label = "keys",
                                )
                                Box {
                                    Column(
                                        // With the keyboard up it sits on the keyboard; down,
                                        // clear of the screen's edge, as on iOS.
                                        Modifier
                                            .fillMaxWidth()
                                            .graphicsLayer { alpha = keysAlpha }
                                            .padding(top = 6.dp, bottom = if (keyboardUp) 6.dp else 20.dp),
                                        verticalArrangement = Arrangement.spacedBy(8.dp),
                                    ) {
                                        UploadStrip(model, pageGlass)
                                        PaneSwitcher(model, pageGlass) { model.onHideKeyboard?.invoke(); showPanes = true }
                                        KeyBar(
                                            model = model,
                                            keyboardUp = keyboardUp,
                                            glass = pageGlass,
                                            onOpenSettings = { showSettings = true },
                                            hardKeyboard = hardKeyboard,
                                        )
                                    }
                                    if (covered || held) {
                                        Box(
                                            Modifier.matchParentSize().pointerInput(Unit) {
                                                awaitPointerEventScope {
                                                    while (true) awaitPointerEvent().changes.forEach { it.consume() }
                                                }
                                            },
                                        )
                                    }
                                }
                            }
                        }

                        // The card's own header rides along with the terminal: laid out at
                        // the card's size, blown up to the terminal's width, and shrunk with
                        // the same zoom, so the whole card closes in, not just its bottom.
                        val hero = zoomTarget
                        val heroThread = heroThread
                        if (hero != null && heroThread != null && terminalBounds.width > 0) {
                            val sEnd = hero.thumb.width / terminalBounds.width
                            val headerPx = hero.thumb.top - hero.card.top
                            val cardW = with(density) { hero.card.width.toDp() }
                            val headerH = with(density) { headerPx.toDp() }
                            Box(
                                Modifier
                                    .zIndex(2f)
                                    .requiredSize(cardW, headerH)
                                    .graphicsLayer {
                                        val t = zoom.value
                                        val scale = (1f + (sEnd - 1f) * t) / sEnd
                                        transformOrigin = TransformOrigin(0f, 0f)
                                        scaleX = scale
                                        scaleY = scale
                                        // Its bottom sits on the slice of terminal the card keeps.
                                        val thumbTop = terminalBounds.top + (hero.thumb.top - terminalBounds.top) * t
                                        translationX = terminalBounds.left + (hero.thumb.left - terminalBounds.left) * t - boxOrigin.x
                                        translationY = thumbTop - headerPx * scale - boxOrigin.y
                                        alpha = t * (1f - fade.value)
                                        val r = 13.dp.toPx() * t
                                        shape = RoundedCornerShape(topStart = r, topEnd = r)
                                        clip = true
                                    }
                                    .background(Color(0xFF1A1C21))
                            ) {
                                ThreadCardHeader(heroThread, MaterialTheme.colorScheme.primary, Modifier.fillMaxWidth())
                            }
                            // The moving card's border, around header and terminal slice
                            // together; the card's own border waits until it has landed.
                            val accent = MaterialTheme.colorScheme.primary
                            Spacer(
                                Modifier
                                    .fillMaxSize()
                                    .zIndex(2.5f)
                                    .drawWithContent {
                                        val t = zoom.value
                                        val a = t * (1f - fade.value)
                                        if (a <= 0f) return@drawWithContent
                                        val scale = 1f + (sEnd - 1f) * t
                                        val top = (terminalBounds.height - hero.thumb.height / sEnd) * t
                                        val left = terminalBounds.left + (hero.thumb.left - terminalBounds.left) * t - boxOrigin.x
                                        val thumbTop = terminalBounds.top + (hero.thumb.top - terminalBounds.top) * t - boxOrigin.y
                                        // The header is laid out at the card's size and blown
                                        // up by 1/sEnd before the zoom scales it.
                                        val headerH = headerPx * scale / sEnd
                                        val frameTop = thumbTop - headerH
                                        val w = terminalBounds.width * scale
                                        val h = headerH + (terminalBounds.height - top) * scale
                                        val r = 13.dp.toPx() * t
                                        val stroke = 2.dp.toPx()
                                        drawRoundRect(
                                            color = accent.copy(alpha = accent.alpha * a),
                                            topLeft = Offset(left + stroke / 2, frameTop + stroke / 2),
                                            size = androidx.compose.ui.geometry.Size(w - stroke, h - stroke),
                                            cornerRadius = androidx.compose.ui.geometry.CornerRadius(r, r),
                                            style = androidx.compose.ui.graphics.drawscope.Stroke(stroke),
                                        )
                                    }
                            )
                        }

                        // Over the whole page too, under the pane map.
                        Box(Modifier.fillMaxSize().zIndex(2.6f)) {
                            ConnectionPage(model, overviewOpen, onEditHost = { editingHost = model.host }, onBack = { model.disconnect(); onBack() })
                        }
                        if (showPanes) {
                            // Over the whole page, the bars at its foot included.
                            Box(Modifier.fillMaxSize().zIndex(2.7f)) {
                                PaneMap(model, glass) { showPanes = false }
                            }
                        }
                    }
                }
                if (moved) {
                    // What shows of the screen takes it back; fainter on a
                    // light page, where the dark one's shade reads as a veil.
                    // It darkens as far as the screen has slid off a sidebar,
                    // the slide's spring and a finger's drag alike.
                    Box(
                        Modifier
                            .matchParentSize()
                            .graphicsLayer { alpha = maxOf(leftSlide.value, rightSlide.value).coerceIn(0f, 1f) }
                            .background(Color.Black.copy(alpha = if (chrome.dark) 0.35f else 0.08f))
                            .clickable(interactionSource = null, indication = null) {
                                showTree = false
                                showRight = false
                            }
                    )
                }
            }
            // Over the whole display, not the screen that slides off the
            // sidebar: the settings pushed in from the right, the host's
            // editor up from the bottom as a sheet.
            AnimatedVisibility(
                showSettings,
                enter = slideInHorizontally(tween(380, easing = FastOutSlowInEasing)) { it },
                exit = slideOutHorizontally(tween(340, easing = FastOutSlowInEasing)) { it },
                modifier = Modifier.zIndex(3f),
            ) {
                MaterialTheme(colorScheme = appScheme) {
                    Surface(Modifier.fillMaxSize(), color = MaterialTheme.colorScheme.background) {
                        SettingsScreen(model = model, showLog = showLog, onDone = { showSettings = false })
                    }
                }
            }
            // The host shown while the editor slides away.
            var lastEdited by remember { mutableStateOf<Host?>(null) }
            editingHost?.let { lastEdited = it }
            AnimatedVisibility(
                editingHost != null,
                enter = slideInVertically(tween(360)) { it },
                exit = slideOutVertically(tween(320)) { it },
                modifier = Modifier.zIndex(3f),
            ) {
                val h = lastEdited ?: return@AnimatedVisibility
                MaterialTheme(colorScheme = appScheme) {
                    Surface(Modifier.fillMaxSize(), color = MaterialTheme.colorScheme.background) {
                        HostEditScreen(
                            store = store,
                            host = h,
                            onSave = { edited, secret, passphrase ->
                                // As stored: a new address drops the old pin.
                                val before = store.hosts.firstOrNull { it.id == edited.id }
                                val saved = store.saveEdit(edited, secret, passphrase)
                                val credentialsChanged = secret.isNotEmpty() || passphrase != null
                                model.applyHostEdit(saved, credentialsChanged, connectOnSave = true)
                                jumpHostKept(store, before, saved, credentialsChanged)
                                editingHost = null
                            },
                            onCancel = { editingHost = null },
                        )
                    }
                }
            }
        }
    }
}

/// The sidebar's width as a column beside the terminal, as iOS's.
private val SIDEBAR_COLUMN = 280.dp

// MARK: the bar

/// One row, as iOS has it: the sidebar's button, the thread's tabs as
/// glass capsules -- the card of what each runs, then its name -- with a
/// new tab's button after them, and the right sidebar's button. The
/// threads are the sidebar's.
@Composable
private fun TopBar(
    model: TerminalModel,
    glass: com.kyant.backdrop.Backdrop,
    overview: Boolean,
    onTree: () -> Unit,
    onRight: () -> Unit,
    onCloseOverview: () -> Unit,
) {
    if (overview) {
        Row(
            Modifier.fillMaxWidth().height(54.dp).padding(horizontal = BAR_EDGE),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            BarCircle("panel-left", glass, onTree)
            Text(
                tr("overview"),
                color = Color.White,
                fontWeight = FontWeight.SemiBold,
                fontSize = 17.sp,
                maxLines = 1,
                modifier = Modifier.weight(1f).padding(start = 4.dp),
            )
            BarCircle("x", glass, onCloseOverview)
        }
        return
    }
    // The tabs have the whole row, scrolling under the round buttons at its
    // ends, as iOS's do: the shadows of their glass are whole, not cut off
    // square where a strip between the buttons would end.
    Box(Modifier.fillMaxWidth().height(54.dp)) {
        TabStrip(model, glass, inset = BAR_EDGE + BAR_CIRCLE + 8.dp, Modifier.fillMaxSize())
        Row(
            Modifier.fillMaxSize().padding(horizontal = BAR_EDGE),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            BarCircle("panel-left", glass, onTree)
            Spacer(Modifier.weight(1f))
            BarCircle("panel-right", glass, onRight)
        }
    }
}

/// The bar's round buttons, and their room from the screen's edge.
private val BAR_CIRCLE = 42.dp
private val BAR_EDGE = 10.dp
/// How far tabs fade before a round button they scroll under.
private val TAB_FADE = 24.dp

/// A round glass button of the bar's.
@Composable
private fun BarCircle(icon: String, glass: com.kyant.backdrop.Backdrop, onClick: () -> Unit) {
    Box(
        Modifier
            .size(BAR_CIRCLE)
            .glass(glass, LocalChrome.current.dark, CircleShape)
            .clickable(onClick = onClick),
        contentAlignment = Alignment.Center,
    ) {
        DesktopIcon(icon, size = 20.dp, tint = ink)
    }
}

/// The thread's tabs, scrolling sideways across the whole bar, `inset` from
/// either end at rest -- clear of its round buttons, which they scroll
/// under. Past a button they fade into the bar before they reach it.
@Composable
private fun TabStrip(model: TerminalModel, glass: com.kyant.backdrop.Backdrop, inset: Dp, modifier: Modifier) {
    val scroll = rememberScrollState()
    val positions = remember { mutableStateMapOf<String, Int>() }
    ScrollToCurrent(scroll, positions, model.tabs?.current?.tab?.let { "tab:$it" })
    val ground = model.background
    // The fades are as high as the bar; the tabs keep to its middle, on
    // the line of the round buttons either side.
    Box(modifier, contentAlignment = Alignment.CenterStart) {
        Row(
            Modifier.horizontalScroll(scroll).padding(horizontal = inset),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            for (tab in model.tabs?.tabs ?: emptyList()) {
                Box(Modifier.onGloballyPositioned { positions["tab:" + tab.tab] = it.positionInParent().x.toInt() }) {
                    TabPill(model, tab, glass)
                }
            }
            NewTabCircle(model, glass)
        }
        // At an end with tabs scrolled past it: the bar's ground under its
        // round button, then a fade into the tabs -- as strong as they have
        // gone past, up to the fade's width, so it comes in with the scroll.
        val solid = inset / (inset + TAB_FADE)
        Box(
            Modifier.align(Alignment.CenterStart).width(inset + TAB_FADE).fillMaxHeight()
                .graphicsLayer { alpha = (scroll.value / TAB_FADE.toPx()).coerceIn(0f, 1f) }
                .background(Brush.horizontalGradient(0f to ground, solid to ground, 1f to ground.copy(alpha = 0f)))
        )
        Box(
            Modifier.align(Alignment.CenterEnd).width(inset + TAB_FADE).fillMaxHeight()
                .graphicsLayer { alpha = ((scroll.maxValue - scroll.value) / TAB_FADE.toPx()).coerceIn(0f, 1f) }
                .background(Brush.horizontalGradient(0f to ground.copy(alpha = 0f), 1f - solid to ground, 1f to ground))
        )
    }
}

/// A tab as the desktop's capsule: the card of what it runs at the left,
/// round within the capsule's round end, then its name; the one on show
/// lit, with the × the desktop's tabs carry.
@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun TabPill(model: TerminalModel, tab: TabEntry, glass: com.kyant.backdrop.Backdrop) {
    val chrome = LocalChrome.current
    var menu by remember(tab.tab) { mutableStateOf(false) }
    // A window tab wears the terminal's card, as on the desktop; what each
    // pane runs is on its capsule.
    val icon = model.tabs?.icon
    Box {
        Row(
            Modifier
                .height(42.dp)
                .widthIn(min = 76.dp, max = 180.dp)
                .glass(glass, chrome.dark, CircleShape, tint = if (tab.current) chrome.ink.copy(alpha = 0.18f) else null)
                .combinedClickable(onClick = { model.chromeClick("pane", pane = tab.target) }, onLongClick = { menu = true })
                .padding(start = if (icon == null) 14.dp else 6.dp, end = if (tab.current) 6.dp else 14.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            if (icon != null) PaneCard(model, icon, 30.dp, busy = false, dark = chrome.dark)
            Text(
                tab.label.ifEmpty { tr("tab.num", tab.tab) },
                color = if (tab.current) chrome.ink else chrome.ink.copy(alpha = 0.62f),
                fontSize = 14.sp,
                fontWeight = if (tab.current) FontWeight.SemiBold else FontWeight.Normal,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
                modifier = Modifier.weight(1f, fill = false),
            )
            if (tab.current) CloseX { model.chromeClick("close-tab", tab = tab.tab) }
        }
        AppContextMenu(model, "tab", tab.tab.toString(), expanded = menu, onDismiss = { menu = false })
    }
}

/// "+" opens a tab; held, it offers the splits too.
@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun NewTabCircle(model: TerminalModel, glass: com.kyant.backdrop.Backdrop) {
    var menu by remember { mutableStateOf(false) }
    Box {
        Box(
            Modifier
                .size(42.dp)
                .glass(glass, LocalChrome.current.dark, CircleShape)
                .combinedClickable(onClick = { model.chromeClick("new-tab") }, onLongClick = { menu = true }),
            contentAlignment = Alignment.Center,
        ) {
            DesktopIcon("plus", size = 20.dp, tint = ink)
        }
        DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
            DropdownMenuItem(text = { Text(tr("newtab")) }, onClick = { model.chromeClick("new-tab"); menu = false })
            DropdownMenuItem(text = { Text(tr("split.right")) }, onClick = { model.chromeClick("split-right"); menu = false })
            DropdownMenuItem(text = { Text(tr("split.below")) }, onClick = { model.chromeClick("split-below"); menu = false })
        }
    }
}

/// The × at the end of the current pill: what the desktop's tabs carry.
@Composable
private fun CloseX(onClick: () -> Unit) {
    Box(Modifier.size(30.dp).clip(CircleShape).clickable(onClick = onClick), contentAlignment = Alignment.Center) {
        DesktopIcon("x", size = 13.dp, tint = ink.copy(alpha = 0.7f))
    }
}

/// What the ⋯ menu used to hold about the host: reconnect or disconnect,
/// and the host's editor.
@Composable
fun ConnectionMenu(model: TerminalModel, expanded: Boolean, onDismiss: () -> Unit, onEditHost: () -> Unit) {
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

/// The strip follows the current pill: it scrolls so the pill sits a
/// third of the way in, as the iOS strips do.
@Composable
private fun ScrollToCurrent(scroll: androidx.compose.foundation.ScrollState, positions: Map<String, Int>, key: String?) {
    val x = key?.let { positions[it] }
    LaunchedEffect(key, x) {
        if (x != null) scroll.animateScrollTo((x - scroll.viewportSize / 3).coerceAtLeast(0))
    }
}

fun statusColor(model: TerminalModel): Color = when {
    // A redial in its first quiet moments shows as the terminal did.
    model.reconnecting && !model.redialQuiet -> Color(0xFFFF9F0A)
    model.isConnected -> Color(0xFF34C759)
    model.connection.startsWith("connecting") || model.connection.contains("reconnect") -> Color(0xFFFF9F0A)
    else -> Color(0xFFFF453A)
}

// MARK: over the terminal

/// One of the copy and paste chip's two: its icon and its word, greyed and
/// not to be pressed while it has nothing to act on.
@Composable
private fun ClipChoice(icon: String, label: String, enabled: Boolean = true, onClick: () -> Unit) {
    val tint = if (enabled) ink else ink.copy(alpha = 0.35f)
    Row(
        Modifier
            .clickable(enabled = enabled, onClick = onClick)
            .padding(horizontal = 12.dp, vertical = 7.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        DesktopIcon(icon, size = 15.dp, tint = tint)
        Text(label, color = tint, fontSize = 13.sp, modifier = Modifier.padding(start = 6.dp))
    }
}

/// The bars over split panes, the scrollbar, the stats, the composition,
/// the take-over card, the log, the selection's copy chip, and the card
/// while there is no terminal to show.
@Composable
private fun BoxScope.TerminalOverlays(model: TerminalModel, showLog: Boolean) {
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
    if (GlassProbe.on) {
        Text(
            "Glass",
            color = ink,
            fontSize = 15.sp,
            fontWeight = FontWeight.SemiBold,
            textAlign = TextAlign.Center,
            modifier = Modifier
                .align(Alignment.BottomEnd)
                .padding(16.dp)
                .glass(LocalGlass.current, LocalChrome.current.dark, CircleShape)
                .width(132.dp)
                .padding(vertical = 13.dp),
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
        // The chip sits over the head cell, a row above it when it fits,
        // and stays inside the terminal at its right edge.
        val top = (y - 40).coerceAtLeast(0.0)
        Row(
            Modifier
                .layout { measurable, constraints ->
                    val chip = measurable.measure(constraints.copy(minWidth = 0, minHeight = 0))
                    val most = (constraints.maxWidth - chip.width).coerceAtLeast(0)
                    layout(chip.width, chip.height) {
                        chip.place(x.dp.roundToPx().coerceIn(0, most), top.dp.roundToPx())
                    }
                }
                .glass(LocalGlass.current, LocalChrome.current.dark, RoundedCornerShape(8.dp)),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            ClipChoice("copy", tr("copy"), enabled = model.selectionMenuCopies) { model.copyFromMenu() }
            ClipChoice("clipboard-paste", tr("paste")) { model.pasteFromMenu() }
        }
    }
    model.status?.card?.let { card ->
        TakeOverBanner(model, card, Modifier.align(Alignment.BottomCenter))
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
    model.pendingHostKey?.let { challenge ->
        androidx.compose.material3.AlertDialog(
            onDismissRequest = { model.disconnect() },
            title = { Text(tr(if (challenge.jump) "hostkey.approve.jumptitle" else "hostkey.approve.title")) },
            text = {
                Column(verticalArrangement = Arrangement.spacedBy(16.dp), modifier = Modifier.verticalScroll(rememberScrollState())) {
                    if (challenge.jump) Text(tr("hostkey.approve.jumpwho", challenge.endpoint.display))
                    Text("${challenge.endpoint.hostname}:${challenge.endpoint.port}")
                    androidx.compose.foundation.text.selection.SelectionContainer {
                        Text(challenge.fingerprint, fontFamily = FontFamily.Monospace)
                    }
                    Text(tr("hostkey.approve.body"))
                }
            },
            confirmButton = { TextButton(onClick = { model.approveHostKey(challenge) }) { Text(tr("hostkey.approve.accept")) } },
            dismissButton = { TextButton(onClick = { model.disconnect() }) { Text(tr("cancel")) } },
        )
    }
    val phase = ConnectionPhase.of(model.connection)
    // Back from the background, the first moments of the dial back show the
    // last frame and a line over it (`StatusCapsule`). It is not live yet,
    // so it is dimmed, and a touch on it goes nowhere, as the card would
    // have it.
    if (model.cardHeld && phase != null && phase !is ConnectionPhase.Failed) {
        Box(
            Modifier
                .fillMaxSize()
                .background(Color.Black.copy(alpha = 0.3f))
                .pointerInput(Unit) {
                    awaitPointerEventScope {
                        while (true) awaitPointerEvent().changes.forEach { it.consume() }
                    }
                },
        )
    }
}

/// Another device holds the terminal, or none does: a card of glass at the
/// terminal's foot, out of the way of what it shows -- what holds it and
/// what a tap does, then the button that takes it, the card's width. While
/// the server is asked the button waits; refused, the card says so and
/// offers to try again. The iOS app draws the same.
@Composable
private fun TakeOverBanner(model: TerminalModel, card: Card, modifier: Modifier) {
    val chrome = LocalChrome.current
    val taking = card.state == "taking"
    Column(
        modifier
            .padding(12.dp)
            .widthIn(max = 520.dp)
            .fillMaxWidth()
            .glass(LocalGlass.current, chrome.dark, RoundedCornerShape(26.dp))
            .padding(14.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(12.dp)) {
            Box(
                Modifier.size(40.dp).clip(CircleShape).background(chrome.ink.copy(alpha = 0.1f)),
                contentAlignment = Alignment.Center,
            ) {
                when (card.state) {
                    "taking" -> CircularProgressIndicator(Modifier.size(18.dp), strokeWidth = 2.dp, color = LeadBlue)
                    "refused" -> DesktopIcon("triangle-alert", size = 20.dp, tint = Color(0xFFFF9F0A))
                    "free" -> DesktopIcon("hand", size = 20.dp, tint = chrome.ink)
                    else -> DesktopIcon("monitor-smartphone", size = 20.dp, tint = chrome.ink)
                }
            }
            Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(2.dp)) {
                Text(card.title, color = chrome.ink, fontSize = 15.sp, fontWeight = FontWeight.SemiBold, maxLines = 2, overflow = TextOverflow.Ellipsis)
                Text(card.hint, color = chrome.ink.copy(alpha = 0.62f), fontSize = 13.sp, maxLines = 2, overflow = TextOverflow.Ellipsis)
            }
        }
        Box(
            Modifier
                .fillMaxWidth()
                .height(44.dp)
                .clip(CircleShape)
                .background(if (taking) LeadBlue.copy(alpha = 0.45f) else LeadBlue)
                .clickable(enabled = !taking) { model.takeOver() },
            contentAlignment = Alignment.Center,
        ) {
            Text(card.action, color = Color.White, fontSize = 16.sp, fontWeight = FontWeight.SemiBold, maxLines = 1)
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
                .background(ink.copy(alpha = if (nav.focused) 0.1f else 0.04f))
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
                val tint = if (nav.focused) ink else LocalChrome.current.dim
                for (member in nav.members) {
                    Row(
                        Modifier
                            .clip(RoundedCornerShape(50))
                            .background(if (member.current) ink.copy(alpha = if (nav.focused) 0.22f else 0.12f) else Color.Transparent)
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
                .background(ink.copy(alpha = 0.35f))
        )
    }
}

/// The App's remark, "copied", or the first moments of a dial back from the
/// background: a capsule of glass over the terminal's top, so the terminal
/// keeps its size as it comes and goes. It turns while the connection is on
/// its way back. Where the connection card is up, the card says how the
/// connection is going: the App's sticky word on it is left to the card,
/// and only passing remarks show here.
@Composable
private fun StatusCapsule(model: TerminalModel, modifier: Modifier = Modifier) {
    val phase = ConnectionPhase.of(model.connection)
    val held = model.cardHeld && phase != null && phase !is ConnectionPhase.Failed
    val carded = !held && (phase != null || (model.reconnecting && !model.autoReconnect))
    val text = when {
        model.copied -> tr("copied")
        held -> tr("card.reconnecting", model.host.display)
        carded && model.toastSticky -> ""
        else -> model.toastText ?: ""
    }
    // A redial back within a moment says nothing (`redialQuiet`).
    val showing = text.isNotEmpty() && (model.copied || held || !model.redialQuiet)
    // It turns while the connection is on its way back by itself; over a
    // card, what it says is a remark, not the connection.
    val redialing = model.reconnecting && model.autoReconnect && !carded
    val turning = !model.copied && (held || redialing)
    // What it said last stays on it while it goes.
    var said by remember { mutableStateOf(Pair("", false)) }
    if (showing) said = Pair(text, turning)
    val chrome = LocalChrome.current
    AnimatedVisibility(
        visible = showing,
        modifier = modifier,
        enter = fadeIn(tween(200)) + slideInVertically(tween(200)) { -it / 2 },
        exit = fadeOut(tween(200)) + slideOutVertically(tween(200)) { -it / 2 },
    ) {
        val (words, spinning) = said
        Row(
            Modifier
                .glass(LocalGlass.current, chrome.dark, RoundedCornerShape(20.dp))
                .padding(horizontal = 14.dp, vertical = 9.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            when {
                model.copied -> DesktopIcon("check", size = 15.dp, tint = chrome.ink)
                spinning -> CircularProgressIndicator(Modifier.size(14.dp), strokeWidth = 2.dp, color = chrome.ink)
            }
            Text(words, color = chrome.ink, fontSize = 13.sp, lineHeight = 17.sp, maxLines = 2, overflow = TextOverflow.Ellipsis)
        }
    }
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

/// The connection page, over the whole screen, the bars included: the star
/// sky, and on it how the connection is going (`ConnectionCard`).
@Composable
private fun BoxScope.ConnectionPage(model: TerminalModel, overviewOpen: Boolean, onEditHost: () -> Unit, onBack: () -> Unit) {
    val phase = ConnectionPhase.of(model.connection)
    // The night the connection page sits on, from the moment it covers the
    // terminal. When the page goes because the pane is there, it streaks
    // past to show it, as the app comes out from behind the launch's;
    // otherwise it fades.
    // The overview's cards are what the screen shows then: the page waits
    // until it closes, as the card it replaced did.
    val covered = !overviewOpen && connectionCovers(model)
    var skyUp by remember { mutableStateOf(covered) }
    var skyRushing by remember { mutableStateOf(false) }
    var skyKey by remember { mutableIntStateOf(0) }
    LaunchedEffect(covered) {
        if (covered) {
            // Each page its own sky: one still rushing off, or fading out
            // after the rush, has already cleared.
            skyKey++
            skyRushing = false
            skyUp = true
            // Nothing typed reaches the terminal under the page, and the
            // keyboard would cover the page's buttons.
            model.onHideKeyboard?.invoke()
        } else if (skyUp && model.connection.startsWith("pane ")) {
            skyRushing = true
            delay((LaunchSky.AFTER_RUSH * 1000).toLong())
            skyUp = false
            skyRushing = false
        } else {
            skyUp = false
        }
    }
    AnimatedVisibility(skyUp, enter = EnterTransition.None, exit = fadeOut(tween(250))) {
        key(skyKey) { ConnectionSky(rushing = skyRushing) }
    }
    if (covered && phase == null) {
        // Asked not to redial: the card offers it instead.
        ConnectionCard(
            ConnectionPhase.Disconnected, model.host.display, endpointOf(model.host),
            canEdit = model.host.id != Host.PROBE_ID,
        ) { action ->
            when (action) {
                CardAction.Back -> onBack()
                else -> model.connect()
            }
        }
    }
    if (covered && phase != null) {
        ConnectionCard(
            phase, model.host.display, endpointOf(model.host),
            canEdit = model.host.id != Host.PROBE_ID,
        ) { action ->
            when (action) {
                CardAction.Retry -> model.connect()
                CardAction.ForgetKeyAndRetry -> model.forgetKeyAndRetry((phase as? ConnectionPhase.Failed)?.reason.orEmpty())
                CardAction.Edit -> onEditHost()
                CardAction.Back -> onBack()
            }
        }
    }
}

/// What a connection on its way says over the star sky (`ConnectionSky`,
/// under it in `ConnectionPage`): the host, how far the connection has
/// got, or what stopped it and what to do about it, with the buttons at the
/// foot where a thumb is. The sky is night whatever the terminal's colours,
/// so the words are white. The iOS app draws the same.
@Composable
private fun BoxScope.ConnectionCard(
    phase: ConnectionPhase,
    hostName: String,
    endpoint: String,
    canEdit: Boolean,
    act: (CardAction) -> Unit,
) {
    val shownEndpoint = if (endpoint.isEmpty() && phase is ConnectionPhase.Connecting) phase.target else endpoint
    // Where the connection is, over the host's name: the words of the
    // card's sentence without the name ("Connecting to"), so the name can
    // stand large under them.
    val label = when (phase) {
        is ConnectionPhase.Connecting -> tr("conn.connecting")
        ConnectionPhase.Attaching -> tr("conn.attaching")
        ConnectionPhase.Reconnecting -> tr("conn.reconnecting")
        is ConnectionPhase.Failed -> tr("conn.failed")
        ConnectionPhase.Disconnected -> tr("conn.disconnected")
    }
    // Stars pass behind the words: a soft shadow keeps them clear.
    val clear = TextStyle(shadow = Shadow(Color.Black.copy(alpha = 0.6f), blurRadius = 16f))
    BoxWithConstraints(
        Modifier
            .fillMaxSize()
            // Every touch is the page's: none reaches the terminal under it.
            .pointerInput(Unit) {
                awaitPointerEventScope {
                    while (true) awaitPointerEvent().changes.forEach { it.consume() }
                }
            }
            // The sky runs under the system's bars and the keyboard; the
            // words and the buttons stay clear of them.
            .safeDrawingPadding()
            .padding(horizontal = 28.dp),
        contentAlignment = Alignment.TopCenter,
    ) {
        // Upright there is room to spare and the page spreads out over the
        // screen's height; on its side a phone has not the height for all
        // of it, and the page scrolls rather than push its buttons off.
        Column(
            Modifier
                .widthIn(max = 520.dp)
                .fillMaxWidth()
                .verticalScroll(rememberScrollState())
                .heightIn(min = maxHeight),
        ) {
            Spacer(Modifier.height(24.dp))
            Spacer(Modifier.weight(1f))
            Column(verticalArrangement = Arrangement.spacedBy(22.dp)) {
                // On the way the sky is the picture; stopped, a mark says why.
                when (phase) {
                    is ConnectionPhase.Failed -> Icon(Icons.Default.Warning, contentDescription = null, tint = Amber, modifier = Modifier.size(40.dp))
                    ConnectionPhase.Disconnected -> Icon(Icons.Outlined.PowerOff, contentDescription = null, tint = Color.White.copy(alpha = 0.75f), modifier = Modifier.size(38.dp))
                    else -> {}
                }
                Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
                    Text(
                        label,
                        color = if (phase is ConnectionPhase.Failed) Amber else Color.White.copy(alpha = 0.7f),
                        fontSize = 17.sp,
                        fontWeight = FontWeight.SemiBold,
                        style = clear,
                    )
                    // One line, a long name made smaller rather than broken at
                    // a hyphen.
                    BasicText(
                        hostName,
                        style = clear.copy(color = Color.White, fontWeight = FontWeight.Bold),
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                        autoSize = TextAutoSize.StepBased(minFontSize = 18.sp, maxFontSize = 34.sp),
                    )
                    if (shownEndpoint.isNotEmpty()) {
                        Text(
                            shownEndpoint,
                            color = Color.White.copy(alpha = 0.55f),
                            fontSize = 15.sp,
                            fontFamily = FontFamily.Monospace,
                            maxLines = 1,
                            overflow = TextOverflow.MiddleEllipsis,
                            style = clear,
                        )
                    }
                }
                when (phase) {
                    is ConnectionPhase.Connecting, ConnectionPhase.Reconnecting -> ConnectionSteps(current = 0)
                    ConnectionPhase.Attaching -> ConnectionSteps(current = 1)
                    is ConnectionPhase.Failed -> Column(verticalArrangement = Arrangement.spacedBy(14.dp)) {
                        hintFor(phase.reason)?.let {
                            Text(it, color = Color.White.copy(alpha = 0.85f), fontSize = 17.sp, lineHeight = 23.sp, style = clear)
                        }
                        androidx.compose.foundation.text.selection.SelectionContainer {
                            Text(
                                phase.reason,
                                color = Color.White.copy(alpha = 0.6f),
                                fontSize = 13.sp,
                                lineHeight = 18.sp,
                                fontFamily = FontFamily.Monospace,
                                maxLines = 4,
                                overflow = TextOverflow.Ellipsis,
                                modifier = Modifier
                                    .fillMaxWidth()
                                    .clip(RoundedCornerShape(12.dp))
                                    .background(Color.White.copy(alpha = 0.08f))
                                    .padding(horizontal = 14.dp, vertical = 11.dp),
                            )
                        }
                    }
                    ConnectionPhase.Disconnected -> {}
                }
            }
            // A little above the middle: more room under it than over it.
            Spacer(Modifier.height(28.dp))
            Spacer(Modifier.weight(2f))
            Column(
                Modifier.fillMaxWidth().padding(bottom = 20.dp),
                verticalArrangement = Arrangement.spacedBy(10.dp),
            ) {
                when (phase) {
                    is ConnectionPhase.Failed -> {
                        if (phase.reason.contains("key changed") && canEdit) {
                            PageButton(tr("forgetretry"), lead = true) { act(CardAction.ForgetKeyAndRetry) }
                        } else {
                            PageButton(tr("retry"), lead = true) { act(CardAction.Retry) }
                        }
                        Row(horizontalArrangement = Arrangement.spacedBy(10.dp)) {
                            if (canEdit) PageButton(tr("edithost"), lead = false, Modifier.weight(1f)) { act(CardAction.Edit) }
                            PageButton(tr("back"), lead = false, Modifier.weight(1f)) { act(CardAction.Back) }
                        }
                    }
                    ConnectionPhase.Disconnected -> {
                        PageButton(tr("reconnect"), lead = true) { act(CardAction.Retry) }
                        PageButton(tr("back"), lead = false) { act(CardAction.Back) }
                    }
                    else -> PageButton(tr("back"), lead = false) { act(CardAction.Back) }
                }
            }
        }
    }
}

/// The two steps of a connection, one under the other: done, on its way
/// (turning), or still to come.
@Composable
private fun ConnectionSteps(current: Int) {
    Column(Modifier.padding(top = 4.dp), verticalArrangement = Arrangement.spacedBy(14.dp)) {
        ConnectionStep(tr("conn.step1"), if (current > 0) 2 else 1)
        ConnectionStep(tr("conn.step2"), if (current > 1) 2 else if (current == 1) 1 else 0)
    }
}

/// `state`: 0 still to come, 1 on its way, 2 done.
@Composable
private fun ConnectionStep(label: String, state: Int) {
    Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(12.dp)) {
        Box(Modifier.size(22.dp), contentAlignment = Alignment.Center) {
            when (state) {
                2 -> Icon(Icons.Default.CheckCircle, contentDescription = null, tint = Color(0xFF34C759), modifier = Modifier.size(22.dp))
                1 -> CircularProgressIndicator(Modifier.size(18.dp), color = Color.White, strokeWidth = 2.dp)
                else -> Box(Modifier.size(18.dp).border(1.5.dp, Color.White.copy(alpha = 0.35f), CircleShape))
            }
        }
        Text(
            label,
            color = Color.White.copy(alpha = when (state) { 0 -> 0.4f; 1 -> 1f; else -> 0.7f }),
            fontSize = 17.sp,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
        )
    }
}

/// A button at the page's foot: the lead one in the desktop's blue, the
/// rest a frosted white over the sky.
@Composable
private fun PageButton(label: String, lead: Boolean, modifier: Modifier = Modifier.fillMaxWidth(), onClick: () -> Unit) {
    Box(
        modifier
            .height(if (lead) 54.dp else 50.dp)
            .clip(CircleShape)
            .background(if (lead) LeadBlue else Color.White.copy(alpha = 0.14f))
            .border(1.dp, Color.White.copy(alpha = if (lead) 0.18f else 0.12f), CircleShape)
            .clickable(onClick = onClick),
        contentAlignment = Alignment.Center,
    ) {
        Text(
            label,
            color = Color.White,
            fontSize = 17.sp,
            fontWeight = if (lead) FontWeight.SemiBold else FontWeight.Medium,
            maxLines = 1,
        )
    }
}

/// The amber of a stop, as the desktop's and iOS's.
private val Amber = Color(0xFFFF9F0A)

/// Where a host is dialled, as the connection page names it under the
/// host: the user, the address, and the port when it is not 22.
private fun endpointOf(host: Host): String {
    if (host.hostname.isEmpty()) return ""
    val user = if (host.user.isEmpty()) "" else host.user + "@"
    val port = if (host.port == 22) "" else ":${host.port}"
    return user + host.hostname + port
}

/// The connection page covers the terminal (`ConnectionCard`): the keys
/// under it have nothing to type into.
private fun connectionCovers(model: TerminalModel): Boolean {
    if (model.pausedForBackground) return false
    val phase = ConnectionPhase.of(model.connection)
    return if (phase != null) !model.cardHeld || phase is ConnectionPhase.Failed
    else model.reconnecting && !model.autoReconnect
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
