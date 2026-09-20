package com.roversx.thinkterm

import androidx.activity.compose.BackHandler
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.slideInHorizontally
import androidx.compose.animation.slideOutHorizontally
import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Add
import androidx.compose.material.icons.filled.KeyboardArrowDown
import androidx.compose.material.icons.filled.MoreHoriz
import androidx.compose.material.icons.filled.Settings
import androidx.compose.material.icons.filled.Star
import androidx.compose.material.icons.filled.Window
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.rotate
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp

/// The desktop's sidebar, as the TUI lays it out: a panel from the left
/// with the tree — the host, its Spaces, their projects, their threads —
/// each level folding, New Thread at the top, Settings at the bottom.
/// Everything is named as the desktop names it.
@Composable
fun SidebarDrawer(
    model: TerminalModel,
    visible: Boolean,
    onDismiss: () -> Unit,
    onEditHost: () -> Unit,
    onSettings: () -> Unit,
) {
    BackHandler(enabled = visible) { onDismiss() }
    Box(Modifier.fillMaxSize()) {
        AnimatedVisibility(visible, enter = fadeIn(), exit = fadeOut()) {
            Box(Modifier.fillMaxSize().background(Color.Black.copy(alpha = 0.45f)).clickable(onClick = onDismiss))
        }
        AnimatedVisibility(visible, enter = slideInHorizontally { -it }, exit = slideOutHorizontally { -it }) {
            // A Surface, not a Box: it sets the content colour, so text with
            // no colour of its own is readable on every theme.
            Surface(Modifier.fillMaxHeight().width(300.dp), color = MaterialTheme.colorScheme.surface) {
                SidebarTree(model, onDismiss, onEditHost, onSettings)
            }
        }
    }
}

/// Which nodes are folded; every Space but the one on show starts folded.
private class Folded(model: TerminalModel) {
    val keys = mutableStateListOf<String>().also { keys ->
        for (space in model.tree?.spaces ?: emptyList()) if (!space.current) keys.add("space:" + space.id)
    }

    fun toggle(key: String) { if (!keys.remove(key)) keys.add(key) }
    operator fun contains(key: String) = keys.contains(key)
}

@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun SidebarTree(model: TerminalModel, onDismiss: () -> Unit, onEditHost: () -> Unit, onSettings: () -> Unit) {
    val folded = remember(model) { Folded(model) }
    var connectionMenu by remember { mutableStateOf(false) }
    /// The Space whose menu is up, if any: one flag for all of them
    /// would open every Space's menu at once.
    var spaceMenuFor by remember { mutableStateOf<String?>(null) }
    var renaming by remember { mutableStateOf<Pair<String, String>?>(null) }
    var renameText by remember { mutableStateOf("") }
    val tree = model.tree
    val editing = model.sidebar?.editing
    val error = model.sidebar?.newProjectError
    val secondary = MaterialTheme.colorScheme.onSurfaceVariant
    val accent = MaterialTheme.colorScheme.primary

    LaunchedEffect(Unit) { model.refreshViews() }
    LaunchedEffect(editing) {
        // A new project has no id yet: the same field, asking for the
        // name of what is about to exist.
        if (editing == null || editing.kind == "none" || renaming != null) return@LaunchedEffect
        renameText = ""
        renaming = editing.kind to (editing.id ?: "")
    }

    Column(Modifier.fillMaxSize()) {
        LazyColumn(Modifier.fillMaxWidth().weight(1f)) {
            // The host: the root of the tree, and the connection's actions.
            item {
                Box {
                    TreeRow(
                        depth = 0,
                        expandable = true,
                        collapsed = "host" in folded,
                        onToggle = { folded.toggle("host") },
                        onClick = { connectionMenu = true },
                        onLongClick = { connectionMenu = true },
                        trailing = {
                            IconButton(onClick = { connectionMenu = true }, modifier = Modifier.size(32.dp)) {
                                Icon(Icons.Default.MoreHoriz, contentDescription = null, tint = secondary, modifier = Modifier.size(18.dp))
                            }
                        },
                    ) {
                        Box(Modifier.size(8.dp).background(statusColor(model), CircleShape))
                        Text(model.host.display, fontWeight = FontWeight.SemiBold, maxLines = 1, overflow = TextOverflow.Ellipsis, modifier = Modifier.weight(1f))
                    }
                    ConnectionMenu(model, expanded = connectionMenu, onDismiss = { connectionMenu = false }, onEditHost = onEditHost)
                }
            }
            if ("host" !in folded) {
                item {
                    Row(
                        Modifier.fillMaxWidth().clickable { model.sideClick("new-thread"); onDismiss() }.padding(horizontal = 16.dp, vertical = 10.dp),
                        verticalAlignment = Alignment.CenterVertically,
                        horizontalArrangement = Arrangement.spacedBy(8.dp),
                    ) {
                        Icon(Icons.Default.Add, contentDescription = null, tint = accent, modifier = Modifier.size(18.dp))
                        Text(tr("thread.new"), color = accent)
                    }
                }
                item {
                    Row(
                        Modifier.fillMaxWidth().padding(start = 16.dp, end = 4.dp, top = 6.dp),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        Text(tr("workspaces").uppercase(), fontSize = 11.sp, color = secondary, modifier = Modifier.weight(1f))
                        IconButton(onClick = { model.sideClick("new-project") }, modifier = Modifier.size(32.dp)) {
                            Icon(Icons.Default.Add, contentDescription = tr("project.new"), tint = secondary, modifier = Modifier.size(18.dp))
                        }
                    }
                }
                for (space in tree?.spaces ?: emptyList()) {
                    val spaceKey = "space:" + space.id
                    item(key = spaceKey) {
                        Box {
                            TreeRow(
                                depth = 1,
                                expandable = space.projects.isNotEmpty(),
                                collapsed = spaceKey in folded,
                                onToggle = { folded.toggle(spaceKey) },
                                onClick = {
                                    if (!space.current) model.setSpace(space.id)
                                    if (spaceKey in folded) folded.toggle(spaceKey)
                                },
                                onLongClick = { spaceMenuFor = space.id },
                                trailing = {
                                    IconButton(onClick = { model.setSpace(space.id); model.sideClick("new-project") }, modifier = Modifier.size(32.dp)) {
                                        Icon(Icons.Default.Add, contentDescription = tr("project.new"), tint = secondary, modifier = Modifier.size(16.dp))
                                    }
                                },
                            ) {
                                Text(space.name, fontWeight = if (space.current) FontWeight.SemiBold else FontWeight.Normal, maxLines = 1, overflow = TextOverflow.Ellipsis, modifier = Modifier.weight(1f))
                            }
                            AppContextMenu(model, "space", "", expanded = spaceMenuFor == space.id, onDismiss = { spaceMenuFor = null })
                        }
                    }
                    if (spaceKey in folded) continue
                    for (project in space.projects) {
                        val projectKey = "project:" + project.id
                        item(key = projectKey) {
                            var menu by remember { mutableStateOf(false) }
                            Box {
                                TreeRow(
                                    depth = 2,
                                    expandable = project.threads.isNotEmpty(),
                                    collapsed = projectKey in folded,
                                    onToggle = { folded.toggle(projectKey) },
                                    onClick = { folded.toggle(projectKey) },
                                    onLongClick = { menu = true },
                                    trailing = {
                                        IconButton(onClick = { model.setSpace(space.id); model.sideClick("new-thread", project.id) }, modifier = Modifier.size(32.dp)) {
                                            Icon(Icons.Default.Add, contentDescription = tr("thread.new"), tint = secondary, modifier = Modifier.size(16.dp))
                                        }
                                    },
                                ) {
                                    Text(project.name, maxLines = 1, overflow = TextOverflow.Ellipsis, modifier = Modifier.weight(1f))
                                }
                                AppContextMenu(model, "project", project.id, expanded = menu, onDismiss = { menu = false })
                            }
                        }
                        if (projectKey in folded) continue
                        for (thread in project.threads) {
                            item(key = "thread:" + thread.id) {
                                var menu by remember { mutableStateOf(false) }
                                Box {
                                    TreeRow(
                                        depth = 3,
                                        expandable = false,
                                        collapsed = false,
                                        selected = thread.selected,
                                        onToggle = {},
                                        onClick = { model.openThread(thread.id, space.id); onDismiss() },
                                        onLongClick = { menu = true },
                                    ) {
                                        Box(Modifier.size(8.dp).background(threadColor(thread.status, thread.live), CircleShape))
                                        if (thread.pinned) Icon(Icons.Default.Star, contentDescription = null, tint = secondary, modifier = Modifier.size(12.dp))
                                        Text(
                                            thread.name,
                                            fontWeight = if (thread.unread || thread.selected) FontWeight.SemiBold else FontWeight.Normal,
                                            color = if (thread.live) MaterialTheme.colorScheme.onSurface else secondary,
                                            maxLines = 1,
                                            overflow = TextOverflow.Ellipsis,
                                            modifier = Modifier.weight(1f),
                                        )
                                    }
                                    AppContextMenu(model, "thread", thread.id, expanded = menu, onDismiss = { menu = false })
                                }
                            }
                        }
                    }
                }
                // What the desktop lists after the tree: archived projects of
                // the Space on show, and windows no thread claims.
                val rows = model.sidebar?.rows ?: emptyList()
                val tail = rows.dropWhile { it !is SideRow.Archived && it !is SideRow.Others }
                for (row in tail) {
                    item(key = row.key) {
                        when (row) {
                            is SideRow.Archived -> Row(
                                Modifier.fillMaxWidth().clickable { model.sideClick("toggle-archived", flag = !row.open) }.padding(start = 16.dp, end = 8.dp, top = 8.dp, bottom = 6.dp),
                                verticalAlignment = Alignment.CenterVertically,
                                horizontalArrangement = Arrangement.spacedBy(6.dp),
                            ) {
                                Icon(Icons.Default.KeyboardArrowDown, contentDescription = null, tint = secondary, modifier = Modifier.size(16.dp).rotate(if (row.open) 0f else -90f))
                                Text("${row.label} (${row.count})", fontSize = 12.sp, color = secondary)
                            }
                            is SideRow.Project -> {
                                var menu by remember { mutableStateOf(false) }
                                Box {
                                    TreeRow(depth = 2, expandable = false, collapsed = true, onToggle = {}, onClick = { menu = true }, onLongClick = { menu = true }) {
                                        Text(row.name, color = secondary, maxLines = 1, overflow = TextOverflow.Ellipsis, modifier = Modifier.weight(1f))
                                    }
                                    AppContextMenu(model, "archived-project", row.id, expanded = menu, onDismiss = { menu = false })
                                }
                            }
                            is SideRow.Others -> Text(tr("otherwindows").uppercase(), fontSize = 11.sp, color = secondary, modifier = Modifier.padding(start = 16.dp, top = 10.dp, bottom = 2.dp))
                            is SideRow.Window -> Row(
                                Modifier
                                    .fillMaxWidth()
                                    .background(if (row.selected) accent.copy(alpha = 0.18f) else Color.Transparent)
                                    .clickable { model.sideClick("window", row.id.toString()); onDismiss() }
                                    .padding(horizontal = 16.dp, vertical = 8.dp),
                                verticalAlignment = Alignment.CenterVertically,
                                horizontalArrangement = Arrangement.spacedBy(8.dp),
                            ) {
                                Icon(Icons.Default.Window, contentDescription = null, tint = secondary, modifier = Modifier.size(16.dp))
                                Text(row.title.ifEmpty { tr("window.n", row.id) }, maxLines = 1, overflow = TextOverflow.Ellipsis)
                            }
                            else -> {}
                        }
                    }
                }
                if (!error.isNullOrEmpty()) {
                    item {
                        Text(error, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.error, modifier = Modifier.padding(horizontal = 16.dp, vertical = 8.dp))
                    }
                }
            }
        }
        HorizontalDivider()
        Row(
            Modifier.fillMaxWidth().clickable { onDismiss(); onSettings() }.padding(horizontal = 16.dp, vertical = 12.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            Icon(Icons.Default.Settings, contentDescription = null, tint = secondary, modifier = Modifier.size(18.dp))
            Text(tr("settings"), color = secondary)
        }
    }

    if (renaming != null) {
        AlertDialog(
            onDismissRequest = { model.sideKey("Escape", ""); renaming = null },
            title = { Text(tr(if (renaming?.first == "new-project") "project.new" else "rename")) },
            text = { OutlinedTextField(value = renameText, onValueChange = { renameText = it }, label = { Text(tr("f.name")) }, singleLine = true) },
            confirmButton = { TextButton(onClick = { model.sideKey("Enter", renameText); renaming = null }) { Text(tr("save")) } },
            dismissButton = { TextButton(onClick = { model.sideKey("Escape", ""); renaming = null }) { Text(tr("cancel")) } },
        )
    }
}

/// One row of the tree: indented by its depth, a fold chevron when it
/// has children, its content, and a trailing button.
@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun TreeRow(
    depth: Int,
    expandable: Boolean,
    collapsed: Boolean,
    selected: Boolean = false,
    onToggle: () -> Unit,
    onClick: () -> Unit,
    onLongClick: () -> Unit,
    trailing: @Composable (() -> Unit)? = null,
    content: @Composable androidx.compose.foundation.layout.RowScope.() -> Unit,
) {
    val accent = MaterialTheme.colorScheme.primary
    val secondary = MaterialTheme.colorScheme.onSurfaceVariant
    Row(
        Modifier
            .fillMaxWidth()
            .background(if (selected) accent.copy(alpha = 0.18f) else Color.Transparent)
            .combinedClickable(onClick = onClick, onLongClick = onLongClick)
            .padding(start = 8.dp + 14.dp * depth, end = 4.dp, top = 6.dp, bottom = 6.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        if (expandable) {
            Icon(
                Icons.Default.KeyboardArrowDown,
                contentDescription = null,
                tint = secondary,
                modifier = Modifier.size(20.dp).rotate(if (collapsed) -90f else 0f).clickable(onClick = onToggle),
            )
        } else {
            Spacer(Modifier.width(20.dp))
        }
        content()
        if (trailing != null) trailing()
    }
}
