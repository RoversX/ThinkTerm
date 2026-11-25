package com.roversx.thinkterm

import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
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
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Check
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.TextRange
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.TextFieldValue
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.kyant.backdrop.Backdrop
import com.kyant.backdrop.backdrops.layerBackdrop
import com.kyant.backdrop.backdrops.rememberLayerBackdrop

/// The desktop's sidebar, as the TUI lays it out -- the host, its Spaces,
/// their projects, their threads -- drawn as iOS draws it: large rows with
/// the desktop's line icons, section titles, a pill on the thread on show.
/// On a phone it lies under the screen, which slides off it. Everything is
/// named as the desktop names it. From ios/Sources/Sidebar.swift.
@Composable
fun SidebarPanel(
    model: TerminalModel,
    /// Something was picked: the drawer closes.
    done: () -> Unit,
    onEditHost: () -> Unit,
    onSettings: () -> Unit,
    onOverview: () -> Unit,
    /// Back to the list of every host's threads.
    onLeave: () -> Unit,
    /// The overview is up: its row is lit.
    overviewOn: Boolean,
) {
    val chrome = LocalChrome.current
    val ground = listBackdrop(chrome.dark)
    val spaces = model.tree?.spaces ?: emptyList()
    // Folded nodes; every Space but the one on show starts folded, once
    // the tree is in: the panel is made before it arrives.
    val folded = remember(model) { mutableStateListOf<String>() }
    var foldedSet by remember(model) { mutableStateOf(false) }
    LaunchedEffect(spaces.isEmpty()) {
        if (!foldedSet && spaces.isNotEmpty()) {
            foldedSet = true
            for (space in spaces) if (!space.current) folded.add("space:" + space.id)
        }
    }
    var searching by remember { mutableStateOf(false) }
    var query by remember { mutableStateOf("") }
    val backdrop = rememberLayerBackdrop()
    // The panel's ground as a layer of its own, for the glass that sits on
    // it: the header's buttons, the search field, the thread on show.
    val groundGlass = rememberLayerBackdrop()
    val bars = WindowInsets.statusBars.asPaddingValues()
    val foot = WindowInsets.navigationBars.asPaddingValues()

    LaunchedEffect(Unit) { model.refreshViews() }

    Box(Modifier.fillMaxSize().background(ground)) {
        Box(Modifier.matchParentSize().layerBackdrop(groundGlass).background(ground))
        // The list runs the panel's full height, the host's name at its head
        // scrolling away with it, under the buttons at the top and the
        // bottom, which blur it as it goes there.
        LazyColumn(
            Modifier.fillMaxSize().layerBackdrop(backdrop).background(ground),
            contentPadding = PaddingValues(
                start = 10.dp,
                end = 10.dp,
                top = bars.calculateTopPadding() + 62.dp,
                bottom = 88.dp + foot.calculateBottomPadding(),
            ),
        ) {
            item {
                Column {
                    Box(Modifier.padding(start = 4.dp, end = 4.dp, bottom = 8.dp)) { HostMenu(model, onEditHost) }
                    AnimatedVisibility(searching, enter = fadeIn(), exit = fadeOut()) {
                        PanelSearchField(query, groundGlass) { query = it }
                    }
                }
            }
            item {
                CompositionLocalProvider(LocalRowGlass provides groundGlass) {
                    if (searching) {
                        SearchResults(model, query, chrome.dark, done)
                    } else {
                        Tree(model, folded, chrome.dark, overviewOn, done, onOverview)
                    }
                }
            }
        }
        // The way back and the search, floating over the list: what scrolls
        // under them shows through as it is, in their glass and between
        // them -- no blurred band, which read as a sheet laid over the list.
        Row(
            Modifier
                .fillMaxWidth()
                .padding(start = 14.dp, end = 14.dp, top = 6.dp + bars.calculateTopPadding(), bottom = 12.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            PanelCircle("chevron-left", backdrop, chrome.dark) { onLeave() }
            // With the Spaces as a menu, the Space on show is here, as it
            // heads the desktop's sidebar.
            val shown = spaces.firstOrNull { it.current }
            if (model.settings.spaces == "menu" && spaces.size > 1 && shown != null) {
                Spacer(Modifier.width(10.dp))
                SpaceMenu(model, shown, backdrop, chrome.dark)
            }
            Spacer(Modifier.weight(1f))
            PanelCircle(if (searching) "x" else "search", backdrop, chrome.dark) {
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
                    .padding(start = 14.dp, end = 14.dp, top = 14.dp, bottom = 20.dp + foot.calculateBottomPadding()),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Row(
                    Modifier
                        .height(50.dp)
                        .glass(backdrop, chrome.dark, CircleShape, tint = LeadBlue)
                        .clickable { model.sideClick("new-thread"); done() }
                        .padding(horizontal = 18.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    DesktopIcon("square-pen", size = 20.dp, tint = Color.White)
                    Spacer(Modifier.width(8.dp))
                    Text(tr("thread.new"), color = Color.White, fontWeight = FontWeight.SemiBold, fontSize = 16.sp, maxLines = 1)
                }
                Spacer(Modifier.weight(1f))
                Box(
                    Modifier
                        .size(50.dp)
                        .glass(backdrop, chrome.dark, CircleShape)
                        .clickable { done(); onSettings() },
                    contentAlignment = Alignment.Center,
                ) {
                    DesktopIcon("settings", size = 22.dp, tint = MaterialTheme.colorScheme.onSurface)
                }
            }
        }
    }
    NameAsk(model)
}

@Composable
private fun PanelCircle(icon: String, glass: Backdrop, dark: Boolean, onClick: () -> Unit) {
    Box(
        Modifier.size(44.dp).glass(glass, dark, CircleShape).clickable(onClick = onClick),
        contentAlignment = Alignment.Center,
    ) {
        DesktopIcon(icon, tint = MaterialTheme.colorScheme.onSurface)
    }
}

/// The host, the tree's root and the connection's menu.
@Composable
private fun HostMenu(model: TerminalModel, onEditHost: () -> Unit) {
    var open by remember { mutableStateOf(false) }
    val connected = model.isConnected
    Box(Modifier.padding(start = 6.dp)) {
        Row(Modifier.clickable { open = true }, verticalAlignment = Alignment.CenterVertically) {
            Text(
                model.host.display,
                fontSize = 22.sp,
                fontWeight = FontWeight.Bold,
                color = MaterialTheme.colorScheme.onSurface,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
                modifier = Modifier.weight(1f, fill = false),
            )
            Spacer(Modifier.width(8.dp))
            Box(Modifier.size(8.dp).background(statusColor(model), CircleShape))
        }
        DropdownMenu(expanded = open, onDismissRequest = { open = false }) {
            DropdownMenuItem(
                text = { Text(if (connected) tr("disconnect") else tr("reconnect")) },
                onClick = { open = false; if (connected) model.disconnect() else model.connect() },
            )
            if (model.host.id != Host.PROBE_ID) {
                DropdownMenuItem(text = { Text(tr("edithost")) }, onClick = { open = false; onEditHost() })
            }
        }
    }
}

@Composable
private fun PanelSearchField(query: String, glass: Backdrop, onChange: (String) -> Unit) {
    val focus = remember { FocusRequester() }
    LaunchedEffect(Unit) { focus.requestFocus() }
    Row(
        Modifier
            .padding(start = 4.dp, end = 4.dp, bottom = 8.dp)
            .fillMaxWidth()
            .height(40.dp)
            .glass(glass, LocalChrome.current.dark, CircleShape)
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
                keyboardOptions = KeyboardOptions(autoCorrectEnabled = false, imeAction = ImeAction.Search),
                modifier = Modifier.fillMaxWidth().focusRequester(focus),
            )
        }
    }
}

// MARK: the tree

@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun Tree(
    model: TerminalModel,
    folded: MutableList<String>,
    dark: Boolean,
    overviewOn: Boolean,
    done: () -> Unit,
    onOverview: () -> Unit,
) {
    val spaces = model.tree?.spaces ?: emptyList()
    val settings = model.settings
    fun toggle(key: String) { if (!folded.remove(key)) folded.add(key) }

    Column {
        // The screen shuts the drawer itself, and waits for it.
        HomeRow(icon = "grid-2x2", title = tr("overview"), selected = overviewOn, onClick = onOverview)

        // The Space on show's, as the desktop pins them; they leave their
        // projects' lists.
        val pinned = spaces.filter { it.current }.flatMap { space ->
            space.projects.flatMap { project -> project.threads.filter { it.pinned }.map { Triple(space, project, it) } }
        }
        if (pinned.isNotEmpty()) {
            SectionTitle(tr("pinned"))
            for ((space, project, thread) in pinned) PanelThreadRow(model, thread, space, 0, dark, project.name, done)
        }

        // As a menu, the Space on show heads its projects and holds the
        // others, as the desktop's sidebar has them; as a list, every
        // Space is a row under the title.
        val menu = settings.spaces == "menu" && spaces.size > 1
        val shown = spaces.firstOrNull { it.current }
        var loneMenu by remember { mutableStateOf(false) }
        Box {
            Row(
                Modifier
                    .fillMaxWidth()
                    // A lone Space has no row (below): its menu -- a new
                    // Space, a new name -- is held here.
                    .combinedClickable(onClick = {}, onLongClick = { if (spaces.size == 1) loneMenu = true }),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Box(Modifier.weight(1f)) { SectionTitle(tr("workspaces")) }
                Box(
                    Modifier.size(36.dp).clickable { model.sideClick("new-project") },
                    contentAlignment = Alignment.Center,
                ) {
                    DesktopIcon("project-add", size = 18.dp, tint = MaterialTheme.colorScheme.onSurfaceVariant)
                }
            }
            AppContextMenu(model, "space", "", expanded = loneMenu, onDismiss = { loneMenu = false })
        }
        when {
            // One Space needs no row of its own: its projects stand at the top.
            spaces.size == 1 -> for (project in spaces[0].projects) ProjectRows(model, spaces[0], project, 0, folded, dark, done)
            menu && shown != null -> for (project in shown.projects) ProjectRows(model, shown, project, 0, folded, dark, done)
            else -> for (space in spaces) {
                val key = "space:" + space.id
                var spaceMenu by remember(space.id) { mutableStateOf(false) }
                Box {
                    HomeRow(
                        icon = "layers",
                        title = space.name,
                        bold = space.current,
                        chevron = if (space.projects.isEmpty()) null else key !in folded,
                        onLongClick = { spaceMenu = true },
                    ) {
                        // Another Space is gone to, open; the one on show folds.
                        if (space.current) {
                            toggle(key)
                        } else {
                            model.setSpace(space.id)
                            folded.remove(key)
                        }
                    }
                    AppContextMenu(model, "space", "", expanded = spaceMenu, onDismiss = { spaceMenu = false })
                }
                if (key !in folded) for (project in space.projects) ProjectRows(model, space, project, 1, folded, dark, done)
            }
        }
        TailRows(model, done)
    }
}

/// The Space on show, in glass; its menu is the App's for a Space, which
/// holds the others to go to (the one on show ticked) as well as a new
/// Space and a new name.
@Composable
private fun SpaceMenu(model: TerminalModel, shown: TreeSpace, glass: Backdrop, dark: Boolean) {
    var open by remember { mutableStateOf(false) }
    val secondary = MaterialTheme.colorScheme.onSurfaceVariant
    Box {
        Row(
            Modifier
                .height(44.dp)
                .glass(glass, dark, CircleShape)
                .clickable { open = true }
                .padding(horizontal = 14.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            DesktopIcon("layers", size = 18.dp, tint = secondary)
            Text(
                shown.name,
                fontSize = 16.sp,
                fontWeight = FontWeight.SemiBold,
                color = MaterialTheme.colorScheme.onSurface,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
                modifier = Modifier.widthIn(max = 120.dp),
            )
            DesktopIcon("chevrons-up-down", size = 14.dp, tint = secondary)
        }
        AppContextMenu(model, "space", "", expanded = open, onDismiss = { open = false })
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

@Composable
private fun ProjectRows(
    model: TerminalModel,
    space: TreeSpace,
    project: TreeProject,
    depth: Int,
    folded: MutableList<String>,
    dark: Boolean,
    done: () -> Unit,
) {
    val key = "project:" + project.id
    val isFolded = key in folded
    // Pinned threads have their own rows, the Space on show's only.
    val threads = if (space.current) project.threads.filter { !it.pinned } else project.threads
    var menu by remember(project.id) { mutableStateOf(false) }
    Box {
        HomeRow(
            icon = if (isFolded) "folder" else "folder-open",
            title = project.name,
            depth = depth,
            dim = true,
            trailing = {
                Box(
                    Modifier.size(32.dp).clickable {
                        model.setSpace(space.id)
                        model.sideClick("new-thread", project.id)
                        done()
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
    if (!isFolded) for (thread in threads) PanelThreadRow(model, thread, space, depth, dark, null, done)
}

@Composable
private fun PanelThreadRow(
    model: TerminalModel,
    thread: ThreadRow,
    space: TreeSpace,
    depth: Int,
    dark: Boolean,
    subtitle: String?,
    done: () -> Unit,
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
        ) {
            model.openThread(thread.id, space.id)
            done()
        }
        AppContextMenu(model, "thread", thread.id, expanded = menu, onDismiss = { menu = false })
    }
}

/// What the desktop lists after the tree: archived projects of the Space
/// on show, and windows no thread claims.
@Composable
private fun TailRows(model: TerminalModel, done: () -> Unit) {
    val rows = model.sidebar?.rows ?: emptyList()
    val tail = rows.dropWhile { it !is SideRow.Archived && it !is SideRow.Others }
    for (row in tail) {
        when (row) {
            // The label carries the count.
            is SideRow.Archived -> HomeRow(icon = "archive", title = row.label, dim = true, chevron = row.open) {
                model.sideClick("toggle-archived", flag = !row.open)
            }
            is SideRow.Project -> if (row.archived) {
                var menu by remember(row.id) { mutableStateOf(false) }
                Box {
                    HomeRow(icon = "folder", title = row.name, depth = 1, dim = true, onLongClick = { menu = true }) {}
                    AppContextMenu(model, "archived-project", row.id, expanded = menu, onDismiss = { menu = false })
                }
            }
            is SideRow.Others -> SectionTitle(tr("otherwindows"))
            is SideRow.Window -> HomeRow(
                icon = "app-window",
                title = row.title.ifEmpty { tr("window.n", row.id) },
                selected = row.selected,
            ) {
                model.sideClick("window", row.id.toString())
                done()
            }
            else -> {}
        }
    }
}

/// Every thread whose name holds the query, wherever it is, with the
/// project it is in.
@Composable
private fun SearchResults(model: TerminalModel, query: String, dark: Boolean, done: () -> Unit) {
    val needle = query.trim().lowercase()
    val found = (model.tree?.spaces ?: emptyList()).flatMap { space ->
        space.projects.flatMap { project ->
            project.threads.filter { needle.isEmpty() || it.name.lowercase().contains(needle) }.map { Triple(space, project, it) }
        }
    }
    Column {
        if (found.isEmpty()) {
            Text(tr("thread.find.none"), fontSize = 14.sp, color = MaterialTheme.colorScheme.onSurfaceVariant, modifier = Modifier.padding(12.dp))
        }
        for ((space, project, thread) in found) {
            HomeRow(
                mark = { ThreadMark(thread.markStatus, thread.dot, thread.live, 9.dp, dark) },
                title = thread.name,
                subtitle = project.name,
                selected = thread.selected,
            ) {
                model.openThread(thread.id, space.id)
                done()
            }
        }
    }
}

/// The App asks for a name -- a new project's, or a rename -- through the
/// sidebar's field: here a dialog, wherever the host's threads are listed.
/// A rename starts from the current name, selected. A new project is a
/// folder on the host, ~/dir or /dir: its dialog stays up until the App
/// takes the path, saying inside it why one was refused.
@Composable
internal fun NameAsk(model: TerminalModel) {
    val editing = model.sidebar?.editing
    val refused = model.sidebar?.newProjectError
    var asking by remember { mutableStateOf<String?>(null) }
    var text by remember { mutableStateOf(TextFieldValue("")) }
    LaunchedEffect(editing, refused) {
        if (editing == null || editing.kind == "none") {
            // The path was taken: the project is being made.
            if (asking == "new-project") asking = null
            return@LaunchedEffect
        }
        if (asking != null) return@LaunchedEffect
        // A new project has no id yet: the same field, asking for the
        // path of what is about to exist. One refused after the dialog
        // went comes back as it was typed, to be corrected.
        if (editing.kind != "new-project" || refused == null) {
            val name = editing.name ?: ""
            text = TextFieldValue(name, selection = TextRange(0, name.length))
        }
        asking = editing.kind
    }
    val kind = asking ?: return
    val adding = kind == "new-project"
    AlertDialog(
        onDismissRequest = { model.sideKey("Escape", ""); asking = null },
        title = { Text(tr(if (adding) "project.new" else "rename")) },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(10.dp)) {
                DialogField(text, { text = it }, if (adding) "~/" else tr("f.name"))
                if (adding && !refused.isNullOrEmpty()) {
                    Text(refused, fontSize = 13.sp, color = MaterialTheme.colorScheme.error)
                }
            }
        },
        confirmButton = {
            TextButton(onClick = {
                model.sideKey("Enter", text.text)
                // A path waits for the App's answer; a name is done.
                if (!adding) asking = null
            }) { Text(tr("save")) }
        },
        dismissButton = { TextButton(onClick = { model.sideKey("Escape", ""); asking = null }) { Text(tr("cancel")) } },
    )
}
