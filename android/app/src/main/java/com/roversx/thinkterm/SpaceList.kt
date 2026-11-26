package com.roversx.thinkterm

import android.content.Context
import android.content.pm.ApplicationInfo
import android.os.Build
import androidx.compose.animation.AnimatedContent
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.spring
import androidx.compose.animation.core.tween
import androidx.compose.animation.expandHorizontally
import androidx.compose.animation.expandVertically
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.shrinkHorizontally
import androidx.compose.animation.shrinkVertically
import androidx.compose.animation.slideInHorizontally
import androidx.compose.animation.slideOutHorizontally
import androidx.compose.animation.togetherWith
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.asPaddingValues
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.navigationBars
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBars
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Check
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.key
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.input.pointer.positionChange
import androidx.compose.ui.input.pointer.positionChangeIgnoreConsumed
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.kyant.backdrop.backdrops.layerBackdrop
import com.roversx.thinkterm.core.RepoProject
import com.roversx.thinkterm.core.repoGroups
import com.kyant.backdrop.backdrops.rememberLayerBackdrop
import kotlinx.coroutines.launch
import kotlin.math.abs

/// Where a Space list is: Threads home, the whole screen; or a terminal's
/// sidebar, beside it or under it.
enum class ListPlace { HOME, PANEL }

/// The hosts the lists name, in the hosts list's order: the store's, and
/// on an emulator of a debug build the Mac it runs on ("This Mac").
fun listedHosts(context: Context, store: HostStore): List<Host> {
    val debuggable = (context.applicationInfo.flags and ApplicationInfo.FLAG_DEBUGGABLE) != 0
    val emulator = Build.HARDWARE.contains("ranchu") || Build.HARDWARE.contains("goldfish") || Build.FINGERPRINT.contains("generic")
    val probe = if (debuggable && emulator) Host.probe(context).takeIf { it.user.isNotEmpty() } else null
    return store.hosts + listOfNotNull(probe?.copy(name = tr("thismac")))
}

/// The host whose Space the lists show: the one looked at last (a Space
/// of it picked, its terminal shown), else one whose connection is up,
/// else the first.
fun shownHost(entries: List<Host>): Host? {
    val last = AppSettings.shared.spaceHost
    return entries.firstOrNull { it.id == last }
        ?: entries.firstOrNull { Sessions.existing(it.id)?.listReady() == true }
        ?: entries.firstOrNull()
}

/// Whether a host's tree is the one its server has now: its connection is
/// up, and not lost for good.
internal fun TerminalModel.listReady() = connection.startsWith("pane ") && !(reconnecting && !autoReconnect)

/// The Space a host's lists show: the one on show in its App.
private fun TerminalModel.shownSpace(): TreeSpace? = tree?.spaces?.let { all -> all.firstOrNull { it.current } ?: all.firstOrNull() }

/// The threads the list shows: with the eye on, only the ones the Status
/// panel has something in -- an agent, a program saying how it is doing.
internal fun shownThread(thread: ThreadRow, agentsOnly: Boolean) = !agentsOnly || thread.agent

/// A project has its row unless agents only are shown and it has none.
internal fun listedProject(project: TreeProject, agentsOnly: Boolean) =
    !agentsOnly || project.threads.any { it.agent }

/// Every Space a swipe goes through, in the order the Space menu lists
/// them: host by host, the hosts whose connection is up. (One whose views
/// no list reads now still has its tree from when one did.)
private fun swipeOrder(entries: List<Host>): List<Pair<Host, String>> = entries.flatMap { host ->
    val model = Sessions.existing(host.id)?.takeIf { it.isConnected && !(it.reconnecting && !it.autoReconnect) }
        ?: return@flatMap emptyList()
    (model.tree?.spaces ?: emptyList()).map { host to it.id }
}

/// The threads of one Space -- of any host, picked at the list's head as
/// the desktop's Space menu picks one, or swiped to -- its pinned threads,
/// its projects and their threads, and what the desktop lists after them.
/// Threads home is this list, the whole screen; a terminal's sidebar is
/// this list again, beside the terminal or under it. A search looks
/// through every host's threads.
@Composable
fun SpaceList(
    place: ListPlace,
    store: HostStore,
    /// The host whose Space is shown.
    host: Host?,
    /// A thread (or its chat) is to be shown, on `host`, its model told
    /// already: the host's terminal comes up.
    onOpen: (Host) -> Unit,
    onSettings: () -> Unit,
    /// Threads home: the list of hosts.
    onHosts: (() -> Unit)? = null,
    /// A sidebar: back to Threads home.
    onBack: (() -> Unit)? = null,
    /// A sidebar: the host's editor.
    onEditHost: (() -> Unit)? = null,
    /// A sidebar: the overview's row, lit while it is up, and its tap.
    overview: Pair<Boolean, () -> Unit>? = null,
    /// A sidebar's drawer closes as something is picked.
    done: () -> Unit = {},
    /// The terminal on screen's own session: it reads its views already.
    inFront: TerminalModel? = null,
    /// A sidebar: a Space of another host was picked (or swiped to): that
    /// host's terminal, with its sidebar out.
    onSwitch: ((Host) -> Unit)? = null,
) {
    val context = LocalContext.current
    val dark = LocalChrome.current.dark
    val ground = listBackdrop(dark)
    val settings = AppSettings.get(context)
    val entries = listedHosts(context, store)
    val model = host?.let { Sessions.existing(it.id) }
    var searching by rememberSaveable(place) { mutableStateOf(false) }
    var query by rememberSaveable(place) { mutableStateOf("") }
    val backdrop = rememberLayerBackdrop()
    // The ground as a layer of its own, for the glass that sits on it: the
    // header's buttons, the search field, the thread on show.
    val groundGlass = rememberLayerBackdrop()
    val bars = WindowInsets.statusBars.asPaddingValues()
    val foot = WindowInsets.navigationBars.asPaddingValues()
    val byRepo = settings.byRepo && !searching
    // Folded projects, by id, the list over.
    val folded = remember { mutableStateListOf<String>() }

    // Threads home reads every host's views, its rows on the screen or
    // not: the Space menu and a search name them all. A sidebar reads the
    // others' only while it looks at them (a search, the menu, the repos),
    // the terminal on show reading its own.
    var menuOpen by remember { mutableStateOf(false) }
    val readAll = place == ListPlace.HOME || searching || menuOpen || byRepo
    for (listed in entries.mapNotNull { Sessions.existing(it.id) }) key(listed) {
        if (listed !== inFront && (readAll || listed === model)) {
            LaunchedEffect(listed) { keepViewsListed(listed) }
        }
        // A rename asked from a row's menu asks for the name here.
        NameAsk(listed)
    }
    LaunchedEffect(model) { model?.refreshViews() }

    fun pick(to: Host, spaceId: String?) {
        val session = Sessions.get(context, store, to)
        val down = !session.isConnected || (session.reconnecting && !session.autoReconnect)
        if (spaceId != null) session.setSpace(spaceId) else if (down) session.connect()
        settings.spaceHost = to.id
        // A sidebar's terminal goes with its Space to the other host.
        if (place == ListPlace.PANEL && to.id != host?.id) onSwitch?.invoke(to)
    }

    fun openRow(on: Host, session: TerminalModel, space: TreeSpace, project: TreeProject, thread: ThreadRow) {
        val chat = thread.chat
        if (settings.chatFirst && chat != null) {
            // Its agent's chat, over the terminal of the agent's pane. From
            // Threads home, back from it is back there. The thread is read
            // and recent all the same, as an opened one is.
            if (!space.current) session.setSpace(space.id)
            session.sideClick("touch-thread", thread.id)
            session.openChat(chat.pane, chat.agent, project.name + " · " + thread.name, fromThreads = place == ListPlace.HOME)
        } else {
            session.openThread(thread.id, space.id)
        }
        settings.spaceHost = on.id
        done()
        onOpen(on)
    }

    BoxWithConstraints(Modifier.fillMaxSize().background(ground)) {
        // On a wide screen Threads home keeps to a readable width, in the
        // middle, as iOS's lists on an iPad.
        val extra = if (place == ListPlace.HOME) ((maxWidth - READABLE_WIDTH) / 2).coerceAtLeast(0.dp) else 0.dp
        Box(Modifier.matchParentSize().layerBackdrop(groundGlass).background(ground))

        // Swiped sideways, the list goes to the Space before or after the
        // one on show, as the desktop's sidebar does: decided as the touch
        // sets off, plainly sideways, so a scroll stays a scroll.
        val spaceId = model?.shownSpace()?.id
        val shownKey = (host?.id ?: "") + "/" + (spaceId ?: "")
        val order = swipeOrder(entries)
        val at = order.indexOfFirst { it.first.id == host?.id && it.second == spaceId }
        val scope = rememberCoroutineScope()
        var drag by remember { androidx.compose.runtime.mutableFloatStateOf(0f) }
        // The spring back of the last swipe, stopped by the next one.
        var settle by remember { mutableStateOf<kotlinx.coroutines.Job?>(null) }
        val pickNow by androidx.compose.runtime.rememberUpdatedState<(Host, String?) -> Unit> { to, space -> pick(to, space) }
        val swipeAt = with(LocalDensity.current) { 64.dp.toPx() }
        val orderNow by androidx.compose.runtime.rememberUpdatedState(order)
        val atNow by androidx.compose.runtime.rememberUpdatedState(at)
        val swipe = Modifier.pointerInput(place) {
            awaitEachGesture {
                val down = awaitFirstDown(requireUnconsumed = false)
                var moved = androidx.compose.ui.geometry.Offset.Zero
                while (true) {
                    val change = awaitPointerEvent().changes.firstOrNull { it.id == down.id } ?: return@awaitEachGesture
                    if (!change.pressed || change.isConsumed) return@awaitEachGesture
                    moved += change.positionChange()
                    if (moved.getDistance() >= viewConfiguration.touchSlop * 2) {
                        if (abs(moved.x) < abs(moved.y) * 2f || orderNow.size < 2 || atNow < 0) return@awaitEachGesture
                        change.consume()
                        break
                    }
                }
                settle?.cancel()
                // Followed to its end by hand: the drag helpers give up on a
                // pointer whose first move was taken here.
                var dx = 0f
                while (true) {
                    val change = awaitPointerEvent().changes.firstOrNull { it.id == down.id } ?: break
                    if (!change.pressed) break
                    dx += change.positionChangeIgnoreConsumed().x
                    change.consume()
                    drag = dx / 3
                }
                val to = when {
                    dx <= -swipeAt -> atNow + 1
                    dx >= swipeAt -> atNow - 1
                    else -> -1
                }
                settle = scope.launch {
                    androidx.compose.animation.core.animate(drag, 0f, animationSpec = spring(dampingRatio = 0.8f, stiffness = 500f)) { at, _ -> drag = at }
                }
                orderNow.getOrNull(to)?.let { (toHost, toSpace) -> pickNow(toHost, toSpace) }
            }
        }

        AnimatedContent(
            targetState = shownKey,
            transitionSpec = {
                // The Space after the one left comes in from the right.
                val forward = order.indexOfFirst { (it.first.id + "/" + it.second) == targetState } >=
                    order.indexOfFirst { (it.first.id + "/" + it.second) == initialState }
                val spec = tween<androidx.compose.ui.unit.IntOffset>(320)
                (slideInHorizontally(spec) { if (forward) it / 3 else -it / 3 } + fadeIn(tween(240))) togetherWith
                    (slideOutHorizontally(spec) { if (forward) -it / 3 else it / 3 } + fadeOut(tween(200)))
            },
            modifier = Modifier.fillMaxSize().then(if (searching) Modifier else swipe).graphicsLayer { translationX = drag },
            label = "space",
        ) { key ->
            val (keyHost, keySpace) = key.split("/", limit = 2).let { it[0] to it.getOrElse(1) { "" } }
            val listHost = entries.firstOrNull { it.id == keyHost }
            val listModel = listHost?.let { Sessions.existing(it.id) }
            LazyColumn(
                Modifier.fillMaxSize().layerBackdrop(backdrop).background(ground),
                contentPadding = PaddingValues(
                    start = 10.dp + extra,
                    end = 10.dp + extra,
                    top = bars.calculateTopPadding() + 62.dp,
                    bottom = 88.dp + foot.calculateBottomPadding(),
                ),
            ) {
                item(key = "head") {
                    Column {
                        if (listHost != null) {
                            SpaceTitle(
                                place = place,
                                entries = entries,
                                host = listHost,
                                model = listModel,
                                spaceId = keySpace,
                                onPick = ::pick,
                                onEditHost = onEditHost,
                                onMenu = { menuOpen = it },
                            )
                        } else {
                            Box(Modifier.padding(top = 20.dp)) { HostsEmptyState { onHosts?.invoke() } }
                        }
                        AnimatedVisibility(searching, enter = fadeIn(), exit = fadeOut()) {
                            ListSearchField(query, groundGlass, dark) { query = it }
                        }
                    }
                }
                item(key = "list") {
                    CompositionLocalProvider(LocalRowGlass provides groundGlass) {
                        Column {
                            when {
                                searching -> Found(entries, query, dark, ::openRow)
                                listHost != null -> {
                                    if (byRepo) RepoGroups(entries, dark, onOpen, ::openRow)
                                    SpaceRows(
                                        host = listHost,
                                        model = listModel,
                                        spaceId = keySpace,
                                        dark = dark,
                                        folded = folded,
                                        skipGrouped = byRepo,
                                        overview = overview,
                                        onOpen = { done(); onOpen(listHost) },
                                        onRow = ::openRow,
                                        connect = { pick(listHost, null) },
                                    )
                                }
                            }
                        }
                    }
                }
            }
        }

        // The way back, the eye and the search, floating over the list:
        // what scrolls under them shows through, fading there.
        Row(
            Modifier
                .fillMaxWidth()
                .progressiveBlur(backdrop, ground, top = true)
                .padding(start = 14.dp + extra, end = 14.dp + extra, top = 6.dp + bars.calculateTopPadding(), bottom = 12.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            if (onBack != null) ListCircle("chevron-left", tr("threads"), backdrop, dark) { onBack() }
            Spacer(Modifier.weight(1f))
            // Only the threads the Status panel has something in, or every
            // one; kept from one launch to the next. A search looks through
            // them all, and the eye narrows away as it fades.
            AnimatedVisibility(
                !searching,
                enter = fadeIn() + expandHorizontally(),
                exit = fadeOut() + shrinkHorizontally(),
            ) {
                Row {
                    ListCircle(
                        "eye",
                        tr(if (settings.agentsOnly) "home.agents.all" else "home.agents.only"),
                        backdrop,
                        dark,
                        tint = if (settings.agentsOnly) LeadBlue else null,
                    ) { settings.agentsOnly = !settings.agentsOnly }
                    Spacer(Modifier.width(10.dp))
                }
            }
            ListCircle(if (searching) "x" else "search", tr(if (searching) "cancel" else "search"), backdrop, dark) {
                searching = !searching
                if (!searching) query = ""
            }
        }

        if (!searching) {
            Row(
                Modifier
                    .align(Alignment.BottomCenter)
                    .fillMaxWidth()
                    .progressiveBlur(backdrop, ground, top = false)
                    // Clear of a large rounded screen corner too.
                    .padding(
                        start = 14.dp + extra,
                        end = 14.dp + extra,
                        top = 14.dp,
                        bottom = 20.dp + foot.calculateBottomPadding() +
                            if (place == ListPlace.HOME) cornerClearance(14.dp + extra, 24.dp, 20.dp + foot.calculateBottomPadding()) else 0.dp,
                    ),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                // A thread of its own in the Space on show.
                if (host != null && model?.listReady() == true) {
                    Row(
                        Modifier
                            .height(50.dp)
                            .glass(backdrop, dark, CircleShape, tint = LeadBlue)
                            .clickable {
                                model.sideClick("new-thread")
                                settings.spaceHost = host.id
                                done()
                                onOpen(host)
                            }
                            .padding(horizontal = 18.dp),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        // On the blue, in either lightness.
                        DesktopIcon("square-pen", size = 20.dp, tint = Color.White)
                        Spacer(Modifier.width(8.dp))
                        Text(tr("thread.new"), color = Color.White, fontWeight = FontWeight.SemiBold, fontSize = 16.sp, maxLines = 1)
                    }
                }
                Spacer(Modifier.weight(1f))
                if (onHosts != null) {
                    ListCircle("server", tr("hosts"), backdrop, dark, size = 50, onClick = onHosts)
                    Spacer(Modifier.width(10.dp))
                }
                ListCircle("settings", tr("settings"), backdrop, dark, size = 50) { done(); onSettings() }
            }
        }
    }
}

/// A round glass button carrying one of the desktop's icons; `label` is
/// what a screen reader says for it.
@Composable
private fun ListCircle(
    icon: String,
    label: String,
    backdrop: com.kyant.backdrop.Backdrop,
    dark: Boolean,
    size: Int = 44,
    /// Lit: a button that is on, its icon white on the colour.
    tint: Color? = null,
    onClick: () -> Unit,
) {
    Box(
        Modifier
            .size(size.dp)
            .glass(backdrop, dark, CircleShape, tint = tint)
            .clickable(role = Role.Button, onClick = onClick)
            .semantics { contentDescription = label },
        contentAlignment = Alignment.Center,
    ) {
        DesktopIcon(icon, size = if (size >= 50) 22.dp else 20.dp, tint = if (tint != null) Color.White else MaterialTheme.colorScheme.onSurface)
    }
}

/// The Space on show, the list's head as on the desktop's sidebar: its name
/// large, the host it is on under it, and a tap away the Space menu -- every
/// host's Spaces, host by host, and what the one on show offers.
@Composable
private fun SpaceTitle(
    place: ListPlace,
    entries: List<Host>,
    host: Host,
    model: TerminalModel?,
    spaceId: String,
    onPick: (Host, String?) -> Unit,
    onEditHost: (() -> Unit)?,
    onMenu: (Boolean) -> Unit,
) {
    var open by remember { mutableStateOf(false) }
    val primary = MaterialTheme.colorScheme.onSurface
    val secondary = MaterialTheme.colorScheme.onSurfaceVariant
    val space = model?.tree?.spaces?.firstOrNull { it.id == spaceId }
    fun show(on: Boolean) {
        open = on
        onMenu(on)
    }
    Box(Modifier.padding(start = 6.dp, end = 4.dp, top = 4.dp, bottom = 8.dp)) {
        Column(Modifier.clip(MaterialTheme.shapes.medium).clickable { show(true) }.padding(horizontal = 6.dp, vertical = 4.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(
                    space?.name ?: host.display,
                    fontSize = if (place == ListPlace.HOME) 30.sp else 24.sp,
                    fontWeight = FontWeight.Bold,
                    color = primary,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                    modifier = Modifier.weight(1f, fill = false),
                )
                Spacer(Modifier.width(6.dp))
                DesktopIcon("chevrons-up-down", size = 16.dp, tint = secondary)
            }
            Row(verticalAlignment = Alignment.CenterVertically) {
                val busy = model != null && model.onItsWay()
                if (busy) {
                    CircularProgressIndicator(Modifier.size(10.dp), strokeWidth = 1.5.dp, color = Color(0xFFFF9F0A))
                } else {
                    Box(Modifier.size(8.dp).clip(CircleShape).background(hostDot(model)))
                }
                Spacer(Modifier.width(6.dp))
                Text(host.display, fontSize = 14.sp, color = secondary, maxLines = 1, overflow = TextOverflow.Ellipsis)
            }
        }
        SpaceMenu(entries, host, model, open, onPick, onEditHost) { show(false) }
    }
}

/// The Space menu: each host and its Spaces (the one on show ticked), a
/// host not connected offering to connect; then what the Space on show
/// offers, as its App words it -- a new Space, its own new name, its end --
/// and the host's connection.
@Composable
private fun SpaceMenu(
    entries: List<Host>,
    host: Host,
    model: TerminalModel?,
    expanded: Boolean,
    onPick: (Host, String?) -> Unit,
    onEditHost: (() -> Unit)?,
    onDismiss: () -> Unit,
) {
    val secondary = MaterialTheme.colorScheme.onSurfaceVariant
    // The App's own items for the Space on show, its Spaces aside: they are
    // listed above with every other host's.
    val own = remember(expanded, model) {
        if (!expanded || model?.listReady() != true) emptyList()
        else model.contextMenu("space", "").filter { !it.id.startsWith("space:") && it.kind != "separator" }
    }
    DropdownMenu(expanded = expanded, onDismissRequest = onDismiss) {
        for ((n, entry) in entries.withIndex()) {
            val session = Sessions.existing(entry.id)
            if (n > 0) HorizontalDivider()
            Row(Modifier.padding(start = 16.dp, end = 16.dp, top = 8.dp, bottom = 2.dp), verticalAlignment = Alignment.CenterVertically) {
                Box(Modifier.size(7.dp).clip(CircleShape).background(hostDot(session)))
                Spacer(Modifier.width(7.dp))
                Text(entry.display, style = MaterialTheme.typography.labelMedium, color = secondary, maxLines = 1, overflow = TextOverflow.Ellipsis)
            }
            val spaces = session?.takeIf { it.listReady() }?.tree?.spaces
            if (spaces == null) {
                DropdownMenuItem(
                    text = { Text(tr(if (session?.onItsWay() == true) "connecting" else "connect"), color = secondary) },
                    leadingIcon = { DesktopIcon("link-2", size = 18.dp, tint = secondary) },
                    onClick = { onDismiss(); onPick(entry, null) },
                )
            } else {
                for (space in spaces) {
                    val shown = entry.id == host.id && space.current
                    DropdownMenuItem(
                        text = { Text(space.name, maxLines = 1, overflow = TextOverflow.Ellipsis, fontWeight = if (shown) FontWeight.SemiBold else FontWeight.Normal) },
                        leadingIcon = { DesktopIcon("layers", size = 18.dp, tint = secondary) },
                        trailingIcon = if (shown) ({ Icon(Icons.Filled.Check, contentDescription = null) }) else null,
                        onClick = { onDismiss(); onPick(entry, space.id) },
                    )
                }
            }
        }
        if (model != null) {
            HorizontalDivider()
            for (item in own) {
                DropdownMenuItem(
                    text = { Text(item.label) },
                    leadingIcon = if (item.icon != null) ({ DesktopIcon(item.icon, size = 18.dp, tint = secondary) }) else null,
                    enabled = item.enabled,
                    onClick = { onDismiss(); model.menuAction(item.id) },
                )
            }
            val up = model.isConnected && !(model.reconnecting && !model.autoReconnect)
            DropdownMenuItem(
                text = { Text(if (up) tr("disconnect") else tr("reconnect")) },
                leadingIcon = { DesktopIcon(if (up) "power" else "rotate-ccw", size = 18.dp, tint = secondary) },
                onClick = { onDismiss(); if (up) model.disconnect() else model.connect() },
            )
            if (onEditHost != null && host.id != Host.PROBE_ID) {
                DropdownMenuItem(
                    text = { Text(tr("edithost")) },
                    leadingIcon = { DesktopIcon("pencil", size = 18.dp, tint = secondary) },
                    onClick = { onDismiss(); onEditHost() },
                )
            }
        }
    }
}

/// A host's connection, as its dot says it: up, waiting on the person (a
/// key to trust), failed, or neither.
@Composable
private fun hostDot(model: TerminalModel?): Color = when {
    model == null -> MaterialTheme.colorScheme.onSurface.copy(alpha = 0.25f)
    model.listReady() -> Color(0xFF34C759)
    model.pendingHostKey != null -> Color(0xFFFF9F0A)
    model.connection.startsWith("failed") -> Color(0xFFFF453A)
    else -> MaterialTheme.colorScheme.onSurface.copy(alpha = 0.25f)
}

/// On its way: dialling, redialling, or up and not ready yet.
private fun TerminalModel.onItsWay(): Boolean {
    val lost = reconnecting && !autoReconnect
    val ready = connection.startsWith("pane ") && !lost
    val redialing = reconnecting && autoReconnect
    return pendingHostKey == null && !lost && (redialing || (isConnected && !ready) || connection.contains("reconnect"))
}

/// The Space's rows: the overview (a sidebar's), its pinned threads, its
/// projects and their threads, and what the desktop lists after them --
/// archived projects, windows no thread claims. A host not connected says
/// how its connection stands instead.
@Composable
private fun SpaceRows(
    host: Host,
    model: TerminalModel?,
    spaceId: String,
    dark: Boolean,
    folded: MutableList<String>,
    skipGrouped: Boolean,
    overview: Pair<Boolean, () -> Unit>?,
    onOpen: () -> Unit,
    onRow: (Host, TerminalModel, TreeSpace, TreeProject, ThreadRow) -> Unit,
    connect: () -> Unit,
) {
    val agentsOnly = AppSettings.shared.agentsOnly
    if (model == null) {
        HomeRow(icon = "link-2", title = tr("connect"), dim = true, onClick = connect)
        return
    }
    val lost = model.reconnecting && !model.autoReconnect
    when {
        // The key is shown, and trusted or not, on its screen.
        model.pendingHostKey != null -> {
            HomeRow(icon = "key-round", title = tr("hostkey.approve.title"), bold = true, onClick = onOpen)
            return
        }
        !model.listReady() && model.onItsWay() -> {
            HomeRow(icon = "link-2", title = tr("connecting"), dim = true, onClick = {})
            return
        }
        !model.listReady() || lost -> {
            HomeRow(icon = "rotate-ccw", title = tr("reconnect"), dim = true, onClick = { model.connect() })
            return
        }
    }
    val space = model.tree?.spaces?.firstOrNull { it.id == spaceId } ?: model.shownSpace()
    overview?.let { (on, tap) -> HomeRow(icon = "grid-2x2", title = tr("overview"), selected = on, onClick = tap) }
    if (space == null) return
    // With projects by repository, the ones in a repository are listed in
    // theirs, above, and only the rest here.
    fun listedHere(project: TreeProject) = listedProject(project, agentsOnly) && !(skipGrouped && project.repo?.remote != null)

    // As the desktop pins them: they leave their projects' lists. A
    // repository's are in its group, above, with the rest of its threads.
    val pinned = space.projects
        .filter { !(skipGrouped && it.repo?.remote != null) }
        .flatMap { project -> project.threads.filter { it.pinned }.map { project to it } }
    Shown(pinned.any { shownThread(it.second, agentsOnly) }) {
        SectionTitle(tr("pinned"))
        for ((project, thread) in pinned) key("pin:" + thread.id) {
            Shown(shownThread(thread, agentsOnly)) {
                ThreadItem(model, thread, 0, dark, project.name) { onRow(host, model, space, project, thread) }
            }
        }
    }

    Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
        Box(Modifier.weight(1f)) { SectionTitle(tr("workspaces")) }
        Box(
            Modifier.size(36.dp).clip(CircleShape).clickable { model.sideClick("new-project") },
            contentAlignment = Alignment.Center,
        ) {
            DesktopIcon("project-add", size = 18.dp, tint = MaterialTheme.colorScheme.onSurfaceVariant)
        }
    }
    // Each row's showing is its own, kept by its id: a row added or gone
    // elsewhere in the list does not play another's fold.
    for (project in space.projects) key(project.id) {
        Shown(listedHere(project)) {
            ProjectItem(model, space, project, folded, dark, agentsOnly, onOpen) { thread -> onRow(host, model, space, project, thread) }
        }
    }
    TailRows(model, agentsOnly, onOpen)

    val windows = (model.sidebar?.rows ?: emptyList()).filterIsInstance<SideRow.Window>()
    val none = space.projects.none { listedHere(it) } && (if (agentsOnly) windows.none { it.agent } else windows.isEmpty())
    // Only when the groups do list one of them: with the eye on, a
    // repository's projects without agents are in neither place.
    val above = skipGrouped && space.projects.any { it.repo?.remote != null && listedProject(it, agentsOnly) }
    if (none) {
        Text(
            tr(if (above) "home.repos.above" else if (agentsOnly) "home.agents.none" else "overview.none"),
            fontSize = 14.sp,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            modifier = Modifier.padding(horizontal = 12.dp, vertical = 10.dp),
        )
    }
}

@Composable
private fun SectionTitle(text: String) {
    Text(
        text,
        fontSize = 14.sp,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
        modifier = Modifier.padding(start = 12.dp, end = 12.dp, top = 16.dp, bottom = 2.dp),
    )
}

/// Rows that come and go with the eye in the bar: folding up and fading,
/// what is under them closing up.
@Composable
internal fun Shown(visible: Boolean, content: @Composable () -> Unit) {
    AnimatedVisibility(
        visible,
        enter = expandVertically() + fadeIn(),
        exit = shrinkVertically() + fadeOut(),
    ) { Column { content() } }
}

@Composable
private fun ProjectItem(
    model: TerminalModel,
    space: TreeSpace,
    project: TreeProject,
    folded: MutableList<String>,
    dark: Boolean,
    agentsOnly: Boolean,
    onOpen: () -> Unit,
    onThread: (ThreadRow) -> Unit,
) {
    val key = "project:" + project.id
    val isFolded = key in folded
    var menu by remember(project.id) { mutableStateOf(false) }
    Box {
        HomeRow(
            icon = if (isFolded) "folder" else "folder-open",
            title = project.name,
            dim = true,
            trailing = {
                Box(
                    Modifier.size(32.dp).clip(CircleShape).clickable {
                        model.setSpace(space.id)
                        model.sideClick("new-thread", project.id)
                        onOpen()
                    },
                    contentAlignment = Alignment.Center,
                ) {
                    DesktopIcon("circle-plus", size = 18.dp, tint = MaterialTheme.colorScheme.onSurfaceVariant)
                }
            },
            onLongClick = { menu = true },
        ) {
            if (isFolded) folded.remove(key) else folded.add(key)
        }
        AppContextMenu(model, "project", project.id, expanded = menu, onDismiss = { menu = false })
    }
    if (!isFolded) {
        // Under the project's name, not stepped in: the desktop's rows all
        // start at one edge. Pinned threads are above, in their own list.
        for (thread in project.threads) key(thread.id) {
            Shown(shownThread(thread, agentsOnly) && !thread.pinned) {
                ThreadItem(model, thread, 0, dark, null) { onThread(thread) }
            }
        }
    }
}

@Composable
private fun ThreadItem(
    model: TerminalModel,
    thread: ThreadRow,
    depth: Int,
    dark: Boolean,
    subtitle: String?,
    onClick: () -> Unit,
) {
    var menu by remember(thread.id) { mutableStateOf(false) }
    Box {
        HomeRow(
            lead = { ThreadCards(model.core, thread.icons, 30.dp, dark) },
            mark = { ThreadMark(thread.markStatus, thread.dot, thread.live, 9.dp, dark) },
            title = thread.name,
            subtitle = subtitle,
            depth = depth,
            dim = !thread.live,
            selected = thread.selected,
            onLongClick = { menu = true },
            onClick = onClick,
        )
        AppContextMenu(model, "thread", thread.id, expanded = menu, onDismiss = { menu = false })
    }
}

/// What the desktop lists after the tree: archived projects of the Space
/// on show, and windows no thread claims (with the eye on, the ones an
/// agent runs in).
@Composable
private fun TailRows(model: TerminalModel, agentsOnly: Boolean, onOpen: () -> Unit) {
    val rows = model.sidebar?.rows ?: emptyList()
    val tail = rows.dropWhile { it !is SideRow.Archived && it !is SideRow.Others }
    val anyWindow = tail.any { it is SideRow.Window && (!agentsOnly || it.agent) }
    for (row in tail) {
        when (row) {
            // The label carries the count.
            is SideRow.Archived -> if (!agentsOnly) {
                HomeRow(icon = "archive", title = row.label, dim = true, chevron = row.open) {
                    model.sideClick("toggle-archived", flag = !row.open)
                }
            }
            is SideRow.Project -> if (row.archived && !agentsOnly) key("archived:" + row.id) {
                var menu by remember(row.id) { mutableStateOf(false) }
                Box {
                    HomeRow(icon = "folder", title = row.name, depth = 1, dim = true, onLongClick = { menu = true }) {}
                    AppContextMenu(model, "archived-project", row.id, expanded = menu, onDismiss = { menu = false })
                }
            }
            is SideRow.Others -> if (anyWindow) SectionTitle(tr("otherwindows"))
            is SideRow.Window -> if (!agentsOnly || row.agent) key("window:" + row.id) {
                HomeRow(
                    icon = "app-window",
                    title = row.title.ifEmpty { tr("window.n", row.id) },
                    selected = row.selected,
                ) {
                    model.sideClick("window", row.id.toString())
                    onOpen()
                }
            }
            else -> {}
        }
    }
}

/// Every host's threads whose names hold the query, under the host's name,
/// each with the project it is in; a host with none says nothing.
@Composable
private fun Found(
    entries: List<Host>,
    query: String,
    dark: Boolean,
    onRow: (Host, TerminalModel, TreeSpace, TreeProject, ThreadRow) -> Unit,
) {
    val needle = query.trim().lowercase()
    var any = false
    for (host in entries) {
        val model = Sessions.existing(host.id)?.takeIf { it.listReady() } ?: continue
        val hits = (model.tree?.spaces ?: emptyList()).flatMap { space ->
            space.projects.flatMap { project ->
                project.threads.filter { needle.isEmpty() || it.name.lowercase().contains(needle) }.map { Triple(space, project, it) }
            }
        }
        if (hits.isEmpty()) continue
        any = true
        Text(
            host.display,
            fontSize = 14.sp,
            fontWeight = FontWeight.SemiBold,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            modifier = Modifier.padding(start = 12.dp, end = 12.dp, top = 14.dp, bottom = 2.dp),
        )
        for ((space, project, thread) in hits) key(host.id + "/" + thread.id) {
            ThreadItem(model, thread, 0, dark, listOf(space.name, project.name).joinToString(" · ")) {
                onRow(host, model, space, project, thread)
            }
        }
    }
    if (!any) {
        Text(tr("thread.find.none"), fontSize = 14.sp, color = MaterialTheme.colorScheme.onSurfaceVariant, modifier = Modifier.padding(12.dp))
    }
}

@Composable
private fun ListSearchField(query: String, glass: com.kyant.backdrop.Backdrop, dark: Boolean, onChange: (String) -> Unit) {
    val focus = remember { androidx.compose.ui.focus.FocusRequester() }
    LaunchedEffect(Unit) { focus.requestFocus() }
    Row(
        Modifier
            .padding(start = 4.dp, end = 4.dp, bottom = 8.dp)
            .fillMaxWidth()
            .height(40.dp)
            .glass(glass, dark, CircleShape)
            .padding(horizontal = 14.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        DesktopIcon("search", size = 18.dp, tint = MaterialTheme.colorScheme.onSurfaceVariant)
        Spacer(Modifier.width(8.dp))
        Box(Modifier.weight(1f)) {
            if (query.isEmpty()) {
                Text(tr("thread.find"), color = MaterialTheme.colorScheme.onSurfaceVariant, fontSize = 16.sp, maxLines = 1)
            }
            androidx.compose.foundation.text.BasicTextField(
                value = query,
                onValueChange = onChange,
                singleLine = true,
                textStyle = androidx.compose.ui.text.TextStyle(color = MaterialTheme.colorScheme.onSurface, fontSize = 16.sp),
                cursorBrush = androidx.compose.ui.graphics.SolidColor(MaterialTheme.colorScheme.onSurface),
                keyboardOptions = androidx.compose.foundation.text.KeyboardOptions(
                    capitalization = androidx.compose.ui.text.input.KeyboardCapitalization.None,
                    autoCorrectEnabled = false,
                    imeAction = androidx.compose.ui.text.input.ImeAction.Search,
                ),
                modifier = Modifier.fillMaxWidth().focusRequester(focus),
            )
        }
    }
}

/// How many parts of the app read a host's views now -- its rows in a
/// list, the repository groups showing its projects -- so that one going
/// does not stop the views another still shows. Main thread only.
private val listHolds = HashMap<TerminalModel, Int>()

/// Keep `model`'s views read while this part of the list shows them, and
/// read as the core says they changed (`TerminalModel.setViewsListed`):
/// from the first part that reads them to the last.
internal suspend fun keepViewsListed(model: TerminalModel) {
    listHolds[model] = (listHolds[model] ?: 0) + 1
    model.setViewsListed(true)
    try {
        while (true) {
            kotlinx.coroutines.delay(1000)
            // The terminal's own surface went with its screen, or the
            // last lend was refused: the list's is lent again.
            if (model.generation == 0uL && model.isInFront) model.setViewsListed(true)
        }
    } finally {
        val left = (listHolds[model] ?: 1) - 1
        if (left > 0) {
            listHolds[model] = left
        } else {
            listHolds.remove(model)
            model.setViewsListed(false)
        }
    }
}

/// A project of one host in a repository's group: that host's copy.
private data class RepoCopy(
    val host: Host,
    val model: TerminalModel,
    val space: TreeSpace,
    val project: TreeProject,
    val branch: String?,
)

/// Every connected host's projects that are in a repository with a remote,
/// put together by repository: its name, then the threads of all its
/// copies. The group is the project, so which computer a thread is on is not
/// said -- unless that computer's connection is down -- and its branch only
/// when the group's threads are on more than one. A host not connected yet
/// has its projects here once it is.
@Composable
private fun RepoGroups(
    entries: List<Host>,
    dark: Boolean,
    onOpen: (Host) -> Unit,
    onRow: (Host, TerminalModel, TreeSpace, TreeProject, ThreadRow) -> Unit,
) {
    val agentsOnly = AppSettings.shared.agentsOnly
    val hosts = entries.mapNotNull { host -> Sessions.existing(host.id)?.takeIf { it.listReady() }?.let { host to it } }
    val projects = hosts.flatMap { (host, model) ->
        (model.tree?.spaces ?: emptyList()).flatMap { space ->
            space.projects.mapNotNull { project ->
                val repo = project.repo ?: return@mapNotNull null
                val remote = repo.remote ?: return@mapNotNull null
                RepoProject(host.id, space.id, project.id, remote, repo.branch, repo.worktree)
            }
        }
    }
    val groups = remember(projects) { repoGroups(projects) }
    for (group in groups) key("repo:" + group.remote.lowercase()) {
        val copies = group.projects.mapNotNull { at ->
            val (host, model) = hosts.firstOrNull { it.first.id == at.host } ?: return@mapNotNull null
            val space = model.tree?.spaces?.firstOrNull { it.id == at.space } ?: return@mapNotNull null
            val project = space.projects.firstOrNull { it.id == at.project } ?: return@mapNotNull null
            RepoCopy(host, model, space, project, at.branch)
        }
        val sayBranch = copies
            .filter { copy -> copy.project.threads.any { shownThread(it, agentsOnly) } }
            .mapTo(HashSet()) { it.branch }
            .size > 1
        // Its owner, not where it is hosted, unless asked for or the
        // host is what tells it from another.
        val under = if (AppSettings.shared.repoHost || group.hostNeeded) group.remote else group.owner.ifEmpty { null }
        Shown(copies.any { listedProject(it.project, agentsOnly) }) {
            RepoTitle(group.name, under, copies, onOpen)
            for (copy in copies) for (thread in copy.project.threads) key(copy.host.id + "/" + thread.id) {
                val subtitle = listOfNotNull(
                    if (copy.model.reconnecting) copy.host.display + " " + tr("offline") else null,
                    copy.branch.takeIf { sayBranch },
                ).joinToString(" · ").ifEmpty { null }
                Shown(shownThread(thread, agentsOnly)) {
                    ThreadItem(copy.model, thread, 0, dark, subtitle) { onRow(copy.host, copy.model, copy.space, copy.project, thread) }
                }
            }
        }
    }
}

/// A repository's name, the group's root, with whose it is under it, and
/// the + that starts a thread in it: in its one copy, or in the one picked
/// from a menu -- the only place a computer is named.
@Composable
private fun RepoTitle(name: String, under: String?, copies: List<RepoCopy>, onOpen: (Host) -> Unit) {
    var menu by remember { mutableStateOf(false) }
    fun start(copy: RepoCopy) {
        copy.model.setSpace(copy.space.id)
        copy.model.sideClick("new-thread", copy.project.id)
        AppSettings.shared.spaceHost = copy.host.id
        onOpen(copy.host)
    }
    Row(
        Modifier.fillMaxWidth().padding(start = 12.dp, end = 12.dp, top = 18.dp, bottom = 4.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Column(Modifier.weight(1f)) {
            Text(
                name,
                fontSize = 20.sp,
                fontWeight = FontWeight.Bold,
                color = MaterialTheme.colorScheme.onSurface,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
            if (under != null) {
                Text(
                    under,
                    fontSize = 12.sp,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
        }
        Box {
            Box(
                Modifier.size(32.dp).clip(CircleShape).clickable {
                    if (copies.size == 1) start(copies[0]) else menu = true
                },
                contentAlignment = Alignment.Center,
            ) {
                DesktopIcon("circle-plus", size = 18.dp, tint = MaterialTheme.colorScheme.onSurfaceVariant)
            }
            DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
                for (copy in copies) DropdownMenuItem(
                    text = { Text(listOfNotNull(copy.host.display, copy.branch).joinToString(" · ")) },
                    onClick = {
                        menu = false
                        start(copy)
                    },
                )
            }
        }
    }
}
