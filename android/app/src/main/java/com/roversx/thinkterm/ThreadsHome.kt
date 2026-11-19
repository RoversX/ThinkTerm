package com.roversx.thinkterm

import android.os.Build
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.isSystemInDarkTheme
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
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.navigationBars
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBars
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyListScope
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.LocalContentColor
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.rotate
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.graphics.luminance
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardCapitalization
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.kyant.backdrop.backdrops.layerBackdrop
import com.kyant.backdrop.backdrops.rememberLayerBackdrop
import kotlinx.coroutines.delay

/// The ground of the app's own lists -- the threads, the sidebar -- in the
/// system's lightness, as iOS's `chromeBackdrop`.
fun listBackdrop(dark: Boolean) = if (dark) Color(0xFF121212) else Color(0xFFF2F2F2)

/// The widest a list runs on a wide screen: wider, it keeps to the middle.
val READABLE_WIDTH = 720.dp

/// The blue of the one button that leads (New Thread), in either lightness.
val LeadBlue = Color(0xFF0A84FF)

/// Where the app opens: every host's threads on one screen, each host as
/// the desktop's sidebar lists it -- its projects, their threads with the
/// desktop's marks -- under glass bars for finding a thread and starting
/// one. A thread tapped opens its host's terminal on it; a host this phone
/// has been in before is connected to as the screen comes up, and one it
/// has not waits for a tap. From ios/Sources/ThreadsHome.swift.
@Composable
fun ThreadsHome(
    store: HostStore,
    debuggable: Boolean,
    onOpen: (Host) -> Unit,
    onHosts: () -> Unit,
    onSettings: () -> Unit,
) {
    val context = LocalContext.current
    val dark = isSystemInDarkTheme()
    val ground = listBackdrop(dark)
    var searching by rememberSaveable { mutableStateOf(false) }
    var query by rememberSaveable { mutableStateOf("") }
    // The hosts whose rows say they are connected: a thread can be
    // started on one of them.
    val connectedHosts = remember { mutableStateListOf<String>() }
    val backdrop = rememberLayerBackdrop()

    // The probe's sshd is on the Mac an emulator runs on; a phone has none.
    val probe = remember(context, debuggable) {
        if (debuggable && isEmulator()) Host.probe(context).takeIf { it.user.isNotEmpty() } else null
    }
    val entries = store.hosts + listOfNotNull(probe?.copy(name = tr("thismac")))

    fun model(host: Host) = Sessions.get(context, store, host)

    // Populate threads on arrival for known hosts that allow automatic connections.
    LaunchedEffect(Unit) {
        for (host in entries) {
            if (Sessions.existing(host.id) != null) continue
            if (!host.autoConnect || (host.id != Host.PROBE_ID && host.lastConnected == null)) continue
            model(host).connect()
        }
    }

    val bars = WindowInsets.statusBars.asPaddingValues()
    val foot = WindowInsets.navigationBars.asPaddingValues()
    val headerHeight = 64.dp + (if (searching) 48.dp else 0.dp)

    BoxWithConstraints(Modifier.fillMaxSize().background(ground)) {
        // On a wide screen the list keeps to a readable width, in the
        // middle, as iOS's lists on an iPad.
        val extra = ((maxWidth - READABLE_WIDTH) / 2).coerceAtLeast(0.dp)
        LazyColumn(
            Modifier.fillMaxSize().layerBackdrop(backdrop).background(ground),
            contentPadding = PaddingValues(
                start = 10.dp + extra,
                end = 10.dp + extra,
                top = bars.calculateTopPadding() + headerHeight,
                bottom = foot.calculateBottomPadding() + 88.dp,
            ),
        ) {
            if (entries.isEmpty()) {
                item { Box(Modifier.padding(top = 40.dp)) { HostsEmptyState(onHosts) } }
            }
            for (host in entries) {
                hostThreads(
                    host = host,
                    model = Sessions.existing(host.id),
                    query = if (searching) query else "",
                    dark = dark,
                    connect = { model(host).connect() },
                    open = { onOpen(host) },
                    connected = { up ->
                        if (up) { if (host.id !in connectedHosts) connectedHosts.add(host.id) } else connectedHosts.remove(host.id)
                    },
                )
            }
        }

        // The list runs under the bars, fading there rather than cut.
        Column(
            Modifier
                .fillMaxWidth()
                .progressiveBlur(backdrop, ground, top = true)
                .padding(top = bars.calculateTopPadding(), bottom = 12.dp),
        ) {
            Row(
                Modifier.fillMaxWidth().height(64.dp).padding(start = 20.dp + extra, end = 14.dp + extra),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text(
                    tr("threads"),
                    fontSize = 32.sp,
                    fontWeight = FontWeight.Bold,
                    color = MaterialTheme.colorScheme.onSurface,
                    maxLines = 1,
                    modifier = Modifier.weight(1f),
                )
                GlassCircle(backdrop, dark, 44, if (searching) "x" else "search", tr(if (searching) "cancel" else "search")) {
                    searching = !searching
                    if (!searching) query = ""
                }
            }
            AnimatedVisibility(searching, enter = fadeIn(), exit = fadeOut()) {
                Box(Modifier.padding(horizontal = extra)) {
                    SearchField(query, onChange = { query = it }, backdrop = backdrop, dark = dark)
                }
            }
        }

        if (!searching) {
            Row(
                Modifier
                    .align(Alignment.BottomCenter)
                    .fillMaxWidth()
                    .progressiveBlur(backdrop, ground, top = false)
                    .padding(start = 14.dp + extra, end = 14.dp + extra, top = 14.dp, bottom = 20.dp + foot.calculateBottomPadding()),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                NewThreadButton(
                    hosts = entries.filter { it.id in connectedHosts },
                    backdrop = backdrop,
                    dark = dark,
                ) { host ->
                    model(host).sideClick("new-thread")
                    onOpen(host)
                }
                Spacer(Modifier.weight(1f))
                GlassCircle(backdrop, dark, 50, "server", tr("hosts"), onClick = onHosts)
                Spacer(Modifier.width(10.dp))
                GlassCircle(backdrop, dark, 50, "settings", tr("settings"), onClick = onSettings)
            }
        }
    }
}

private fun isEmulator(): Boolean =
    Build.HARDWARE.contains("ranchu") || Build.HARDWARE.contains("goldfish") || Build.FINGERPRINT.contains("generic")

/// A round glass button carrying one of the desktop's icons; `label` is
/// what a screen reader says for it.
@Composable
private fun GlassCircle(
    backdrop: com.kyant.backdrop.Backdrop,
    dark: Boolean,
    size: Int,
    icon: String,
    label: String,
    onClick: () -> Unit,
) {
    Box(
        Modifier
            .size(size.dp)
            .glass(backdrop, dark, CircleShape)
            .clickable(role = Role.Button, onClick = onClick)
            .semantics { contentDescription = label },
        contentAlignment = Alignment.Center,
    ) {
        DesktopIcon(icon, size = if (size >= 50) 22.dp else 20.dp, tint = MaterialTheme.colorScheme.onSurface)
    }
}

@Composable
private fun SearchField(query: String, onChange: (String) -> Unit, backdrop: com.kyant.backdrop.Backdrop, dark: Boolean) {
    val focus = remember { FocusRequester() }
    LaunchedEffect(Unit) { focus.requestFocus() }
    Row(
        Modifier
            .padding(start = 14.dp, end = 14.dp, bottom = 8.dp)
            .fillMaxWidth()
            .height(40.dp)
            .glass(backdrop, dark, CircleShape)
            .padding(horizontal = 14.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        DesktopIcon("search", size = 18.dp, tint = MaterialTheme.colorScheme.onSurfaceVariant)
        Spacer(Modifier.width(8.dp))
        Box(Modifier.weight(1f)) {
            if (query.isEmpty()) {
                Text(tr("thread.find"), color = MaterialTheme.colorScheme.onSurfaceVariant, fontSize = 16.sp, maxLines = 1)
            }
            BasicTextField(
                value = query,
                onValueChange = onChange,
                singleLine = true,
                textStyle = TextStyle(color = MaterialTheme.colorScheme.onSurface, fontSize = 16.sp),
                cursorBrush = SolidColor(MaterialTheme.colorScheme.onSurface),
                keyboardOptions = KeyboardOptions(
                    capitalization = KeyboardCapitalization.None,
                    autoCorrectEnabled = false,
                    imeAction = ImeAction.Search,
                ),
                modifier = Modifier.fillMaxWidth().focusRequester(focus),
            )
        }
    }
}

/// With one host connected the button starts a thread there; with more it
/// asks which; with none there is nowhere to start one.
@Composable
private fun NewThreadButton(hosts: List<Host>, backdrop: com.kyant.backdrop.Backdrop, dark: Boolean, start: (Host) -> Unit) {
    if (hosts.isEmpty()) return
    var asking by remember { mutableStateOf(false) }
    Box {
        Row(
            Modifier
                .height(50.dp)
                .glass(backdrop, dark, CircleShape, tint = LeadBlue)
                .clickable { if (hosts.size == 1) start(hosts[0]) else asking = true }
                .padding(horizontal = 18.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            // On the blue, in either lightness.
            DesktopIcon("square-pen", size = 20.dp, tint = Color.White)
            Spacer(Modifier.width(8.dp))
            Text(tr("thread.new"), color = Color.White, fontWeight = FontWeight.SemiBold, fontSize = 16.sp, maxLines = 1)
        }
        DropdownMenu(expanded = asking, onDismissRequest = { asking = false }) {
            for (host in hosts) {
                DropdownMenuItem(text = { Text(host.display) }, onClick = { asking = false; start(host) })
            }
        }
    }
}

// MARK: one host

/// One host in the list: its name and how its connection stands, then its
/// projects and their threads as the sidebar draws them. A host with no
/// session offers to connect in their place; while a search is on, only
/// the threads it finds are listed.
private fun LazyListScope.hostThreads(
    host: Host,
    model: TerminalModel?,
    query: String,
    dark: Boolean,
    connect: () -> Unit,
    open: () -> Unit,
    connected: (Boolean) -> Unit,
) {
    if (model == null) {
        if (query.isEmpty()) {
            item(key = "host:" + host.id) {
                Column {
                    HostTitle(host.display, MaterialTheme.colorScheme.onSurface.copy(alpha = 0.25f))
                    HomeRow(icon = "link-2", title = tr("connect"), dim = true, onClick = connect)
                }
            }
        }
        return
    }
    item(key = "host:" + host.id) {
        ConnectedHost(host.display, model, query, dark, connect, open, connected)
    }
}

/// The host's name, the tree's root, with the dot of its connection.
@Composable
private fun HostTitle(name: String, color: Color, busy: Boolean = false, modifier: Modifier = Modifier) {
    Row(
        modifier.fillMaxWidth().padding(start = 12.dp, end = 12.dp, top = 18.dp, bottom = 4.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(
            name,
            fontSize = 20.sp,
            fontWeight = FontWeight.Bold,
            color = MaterialTheme.colorScheme.onSurface,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
            modifier = Modifier.weight(1f, fill = false),
        )
        Spacer(Modifier.width(8.dp))
        if (busy) {
            CircularProgressIndicator(Modifier.size(12.dp), strokeWidth = 1.5.dp, color = Color(0xFFFF9F0A))
        } else {
            Box(Modifier.size(8.dp).clip(CircleShape).background(color))
        }
    }
}

/// A host with a session: what its tree holds, kept current while the list
/// is up -- the session's own frames rest while its terminal is off the
/// screen, so the list reads the views itself.
@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun ConnectedHost(
    name: String,
    model: TerminalModel,
    query: String,
    dark: Boolean,
    connect: () -> Unit,
    open: () -> Unit,
    connected: (Boolean) -> Unit,
) {
    // The App keeps the pane's status while its connection is lost. With
    // the setting off nothing redials it: not ready, not on its way, and
    // the way back is a tap.
    val lost = model.reconnecting && !model.autoReconnect
    val ready = model.connection.startsWith("pane ") && !lost
    val redialing = model.reconnecting && model.autoReconnect
    val connecting = !lost && (redialing || (model.isConnected && !ready) || model.connection.contains("reconnect"))
    val dot = when {
        ready -> Color(0xFF34C759)
        // Waiting on the user, not broken.
        model.pendingHostKey != null -> Color(0xFFFF9F0A)
        model.connection.startsWith("failed") -> Color(0xFFFF453A)
        else -> MaterialTheme.colorScheme.onSurface.copy(alpha = 0.25f)
    }
    LaunchedEffect(ready) { connected(ready) }
    // A rename asked from a row's menu here asks for the name here, not
    // later in the sidebar.
    NameAsk(model)
    LaunchedEffect(model) {
        // The views are kept while the list shows them, and read as the
        // core says they changed (`TerminalModel.setViewsListed`).
        model.setViewsListed(true)
        try {
            while (true) {
                delay(1000)
                // The terminal's own surface went with its screen, or the
                // last lend was refused: the list's is lent again.
                if (model.generation == 0uL && model.isInFront) model.setViewsListed(true)
            }
        } finally {
            model.setViewsListed(false)
        }
    }

    Column {
        if (query.isNotEmpty()) {
            Found(name, model, query, dot, dark, open)
            return@Column
        }
        var menu by remember { mutableStateOf(false) }
        Box {
            HostTitle(
                name,
                dot,
                busy = connecting && model.pendingHostKey == null,
                modifier = Modifier.combinedClickable(onClick = { if (ready) open() }, onLongClick = { menu = true }),
            )
            DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
                val up = model.isConnected && !lost
                DropdownMenuItem(
                    text = { Text(if (up) tr("disconnect") else tr("reconnect")) },
                    onClick = { menu = false; if (up) model.disconnect() else connect() },
                )
            }
        }
        when {
            // The key is shown, and trusted or not, on its screen.
            model.pendingHostKey != null -> HomeRow(icon = "key-round", title = tr("hostkey.approve.title"), bold = true, onClick = open)
            !ready && connecting -> HomeRow(icon = "link-2", title = tr("connecting"), dim = true, onClick = {})
            !ready -> HomeRow(icon = "rotate-ccw", title = tr("reconnect"), dim = true, onClick = connect)
            else -> HostTree(model, dark, open)
        }
    }
}

/// The host's Spaces, projects and threads; a lone Space has no row of its
/// own, as in the sidebar. Windows no thread claims come last.
@Composable
private fun HostTree(model: TerminalModel, dark: Boolean, open: () -> Unit) {
    val spaces = model.tree?.spaces ?: emptyList()
    val folded = remember { mutableStateListOf<String>() }
    val windows = (model.sidebar?.rows ?: emptyList()).filterIsInstance<SideRow.Window>()
    if (spaces.size == 1) {
        for (project in spaces[0].projects) ProjectRows(model, spaces[0], project, 0, folded, dark, open)
    } else {
        for (space in spaces) {
            HomeRow(icon = "layers", title = space.name, bold = space.current, dim = true, onClick = {})
            for (project in space.projects) ProjectRows(model, space, project, 1, folded, dark, open)
        }
    }
    for (window in windows) {
        HomeRow(icon = "app-window", title = window.title.ifEmpty { tr("window.n", window.id) }) {
            model.sideClick("window", window.id.toString())
            open()
        }
    }
    if (spaces.all { it.projects.isEmpty() } && windows.isEmpty()) {
        Text(
            tr("overview.none"),
            fontSize = 14.sp,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            modifier = Modifier.padding(horizontal = 12.dp, vertical = 10.dp),
        )
    }
}

@Composable
private fun ProjectRows(
    model: TerminalModel,
    space: TreeSpace,
    project: TreeProject,
    depth: Int,
    folded: MutableList<String>,
    dark: Boolean,
    open: () -> Unit,
) {
    val key = "project:" + project.id
    val isFolded = key in folded
    var menu by remember(project.id) { mutableStateOf(false) }
    Box {
        HomeRow(
            icon = if (isFolded) "folder" else "folder-open",
            title = project.name,
            depth = depth,
            dim = true,
            trailing = {
                Box(
                    Modifier.size(32.dp).clip(CircleShape).clickable {
                        model.setSpace(space.id)
                        model.sideClick("new-thread", project.id)
                        open()
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
        // start at one edge.
        for (thread in project.threads) ThreadRowItem(model, thread, space, depth, dark, null, open)
    }
}

@Composable
private fun ThreadRowItem(
    model: TerminalModel,
    thread: ThreadRow,
    space: TreeSpace,
    depth: Int,
    dark: Boolean,
    subtitle: String?,
    open: () -> Unit,
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
            onLongClick = { menu = true },
        ) {
            model.openThread(thread.id, space.id)
            open()
        }
        AppContextMenu(model, "thread", thread.id, expanded = menu, onDismiss = { menu = false })
    }
}

/// The threads whose names hold the query, under the host's name, each with
/// the project it is in; a host with none says nothing.
@Composable
private fun Found(name: String, model: TerminalModel, query: String, dot: Color, dark: Boolean, open: () -> Unit) {
    val needle = query.lowercase()
    val hits = (model.tree?.spaces ?: emptyList()).flatMap { space ->
        space.projects.flatMap { project ->
            project.threads.filter { it.name.lowercase().contains(needle) }.map { Triple(space, project, it) }
        }
    }
    if (hits.isEmpty()) return
    HostTitle(name, dot)
    for ((space, project, thread) in hits) ThreadRowItem(model, thread, space, 0, dark, project.name, open)
}

/// A row as the desktop's sidebar draws it: an icon (or a thread's cards),
/// the title, and at the end a thread's mark or a button of its own.
@OptIn(ExperimentalFoundationApi::class)
@Composable
fun HomeRow(
    icon: String? = null,
    lead: (@Composable () -> Unit)? = null,
    mark: (@Composable () -> Unit)? = null,
    title: String,
    subtitle: String? = null,
    depth: Int = 0,
    bold: Boolean = false,
    dim: Boolean = false,
    selected: Boolean = false,
    /// Unfolded (true), folded (false), or nothing to fold (null).
    chevron: Boolean? = null,
    trailing: (@Composable () -> Unit)? = null,
    onLongClick: (() -> Unit)? = null,
    onClick: () -> Unit,
) {
    val primary = MaterialTheme.colorScheme.onSurface
    val secondary = MaterialTheme.colorScheme.onSurfaceVariant
    val rowGlass = LocalRowGlass.current
    Row(
        Modifier
            .fillMaxWidth()
            .heightIn(min = 44.dp)
            // The row on show: the desktop's pill, in glass where the list
            // has a layer behind it. Drawn, not clipped to: a thread's cards
            // cast their shadows past the row's edges.
            .then(
                when {
                    !selected -> Modifier
                    rowGlass != null -> Modifier.glass(rowGlass, MaterialTheme.colorScheme.surface.luminance() < 0.5f, CircleShape)
                    else -> Modifier
                        .background(primary.copy(alpha = 0.13f), CircleShape)
                        .border(1.dp, primary.copy(alpha = 0.1f), CircleShape)
                }
            )
            .combinedClickable(onClick = onClick, onLongClick = onLongClick)
            .padding(start = (12 + depth * 18).dp, end = 10.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        when {
            lead != null -> lead()
            icon != null -> Box(Modifier.width(24.dp), contentAlignment = Alignment.Center) {
                DesktopIcon(icon, tint = secondary)
            }
        }
        Column(Modifier.weight(1f)) {
            Text(
                title,
                fontSize = 16.sp,
                fontWeight = if (bold || lead != null) FontWeight.SemiBold else FontWeight.Normal,
                color = if (dim) secondary else primary,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
            if (subtitle != null) {
                Text(subtitle, fontSize = 13.sp, color = secondary, maxLines = 1, overflow = TextOverflow.Ellipsis)
            }
        }
        if (trailing != null) CompositionLocalProvider(LocalContentColor provides secondary) { trailing() }
        if (mark != null) Box(Modifier.padding(end = 4.dp)) { mark() }
        if (chevron != null) {
            DesktopIcon("chevron-right", size = 14.dp, tint = secondary, modifier = Modifier.rotate(if (chevron) 90f else 0f))
        }
    }
}
