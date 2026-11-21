package com.roversx.thinkterm

import androidx.activity.compose.BackHandler
import androidx.compose.animation.AnimatedContent
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.RowScope
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.luminance
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp

/// The saved hosts, searched and grouped, each a row that opens its own
/// page (`hostPage`, kept by the caller so the way back from its terminal
/// comes back to it); plus -- in a debuggable build -- the entry that
/// reaches the probe sshd on the developer's Mac. Adding a host takes the
/// whole screen as a sheet; system back closes it.
/// Follows ios/Sources/HostsView.swift.
@Composable
fun HostsScreen(
    store: HostStore,
    showProbe: Boolean,
    onBack: (() -> Unit)? = null,
    hostPage: String? = null,
    onHostPage: (String?) -> Unit = {},
    onOpen: (Host) -> Unit,
) {
    val context = LocalContext.current
    val probe = remember(context, showProbe) {
        if (showProbe) Host.probe(context).takeIf { it.user.isNotEmpty() } else null
    }
    var adding by remember { mutableStateOf<Host?>(null) }
    var search by remember { mutableStateOf("") }
    var pendingDelete by remember { mutableStateOf<Host?>(null) }
    // A host others are reached through, asked to be deleted: it is not.
    var jumpInUse by remember { mutableStateOf<Host?>(null) }
    var tailnetOpen by remember { mutableStateOf(false) }
    // A page for a host since removed is no page.
    val paged = hostPage?.let { id -> store.hosts.firstOrNull { it.id == id } }
    val pushed = when {
        paged != null -> "host:" + paged.id
        tailnetOpen -> "tailnet"
        else -> null
    }

    // A host added comes up over the list as a sheet, and goes down again.
    AnimatedContent(
        targetState = adding,
        transitionSpec = { sheetTransition(targetState != null) },
        contentKey = { it?.id },
        label = "host-editor",
    ) { target ->
        if (target != null) {
            BackHandler { adding = null }
            HostEditScreen(
                store = store,
                host = target,
                onSave = { host, secret, passphrase ->
                    keepHost(store, host, secret, passphrase)
                    adding = null
                },
                onCancel = { adding = null },
            )
        } else {
            // A host's page, or the tailnet's settings, pushed over the list.
            AnimatedContent(
                targetState = pushed,
                transitionSpec = { pushTransition(targetState != null) },
                label = "hosts-pushed",
            ) { shown ->
                when {
                    shown == "tailnet" -> {
                        BackHandler { tailnetOpen = false }
                        TailnetSettingsScreen { tailnetOpen = false }
                    }
                    shown != null && paged != null -> {
                        BackHandler { onHostPage(null) }
                        HostEditScreen(
                            store = store,
                            host = paged,
                            onSave = { host, secret, passphrase -> keepHost(store, host, secret, passphrase) },
                            onCancel = { onHostPage(null) },
                            page = HostPageActions(
                                open = onOpen,
                                duplicate = {
                                    store.duplicate(paged, tr("host.copysuffix"))
                                    onHostPage(null)
                                },
                                delete = {
                                    if (store.reachedThrough(paged.id).isNotEmpty()) {
                                        jumpInUse = paged
                                    } else {
                                        Sessions.close(paged.id)
                                        store.remove(paged)
                                        onHostPage(null)
                                    }
                                },
                            ),
                        )
                    }
                    else -> HostList(
                        store, probe, search, { search = it }, onBack, onOpen,
                        page = { onHostPage(it.id) },
                        add = { adding = it },
                        delete = { if (store.reachedThrough(it.id).isNotEmpty()) jumpInUse = it else pendingDelete = it },
                        openTailnet = { tailnetOpen = true },
                    )
                }
            }
        }
    }

    pendingDelete?.let { host ->
        val colors = GroupedColors(MaterialTheme.colorScheme.surface.luminance() < 0.5f)
        AlertDialog(
            onDismissRequest = { pendingDelete = null },
            title = { Text(tr("host.delete.title")) },
            text = { Text(tr("host.delete.body")) },
            confirmButton = {
                TextButton(onClick = { Sessions.close(host.id); store.remove(host); pendingDelete = null }) {
                    Text(tr("host.delete.confirm", host.display), color = colors.destructive)
                }
            },
            dismissButton = {
                TextButton(onClick = { pendingDelete = null }) { Text(tr("cancel")) }
            },
        )
    }

    jumpInUse?.let { host ->
        AlertDialog(
            onDismissRequest = { jumpInUse = null },
            title = { Text(tr("jump.inuse.title")) },
            text = { Text(tr("jump.inuse.body", store.reachedThrough(host.id).joinToString { it.display })) },
            confirmButton = { TextButton(onClick = { jumpInUse = null }) { Text(tr("ok")) } },
        )
    }
}

/// Forgets a host's pinned key, in the store and in a session open on it,
/// whose next connection would otherwise still hold the host to the old
/// one: it asks for the key the host has now.
internal fun forgetHostKey(store: HostStore, id: String) {
    store.forgetHostKey(id)
    // Not an edit of the connection: the one up stays, and one down waits
    // for the user's retry rather than dialling into the key's question.
    Sessions.existing(id)?.forgetHostKey()
    // Those reached through it ask for its key on their next dial too.
    for (behind in store.reachedThrough(id)) Sessions.existing(behind.id)?.forgetJumpHostKey()
}

/// Keeps a host as edited or added; a session open on it takes the edit:
/// another address, login or key is a connection made again.
private fun keepHost(store: HostStore, host: Host, secret: String, passphrase: String?) {
    val before = store.hosts.firstOrNull { it.id == host.id }
    val saved = store.saveEdit(host, secret, passphrase)
    val credentialsChanged = secret.isNotEmpty() || passphrase != null
    Sessions.existing(host.id)?.applyHostEdit(saved, credentialsChanged, connectOnSave = false)
    jumpHostKept(store, before, saved, credentialsChanged)
}

/// A host kept as edited: the sessions reached through it go the new way,
/// when the way to it changed -- not for its command, which they never run.
internal fun jumpHostKept(store: HostStore, before: Host?, saved: Host, credentialsChanged: Boolean) {
    if (before != null && before.sameJumpWay(saved) && before.knownHost == saved.knownHost && !credentialsChanged) return
    for (behind in store.reachedThrough(saved.id)) Sessions.existing(behind.id)?.applyJumpHostEdit()
}

/// A host's address in a row, and what it is reached through.
internal fun hostLine(store: HostStore, host: Host): String =
    store.jumpOf(host)?.let { host.address + "  ·  " + tr("jump.via", it.title) } ?: host.address

/// The hosts themselves: searched, grouped, each a row that opens its page.
@Composable
private fun HostList(
    store: HostStore,
    probe: Host?,
    search: String,
    onSearch: (String) -> Unit,
    onBack: (() -> Unit)?,
    onOpen: (Host) -> Unit,
    page: (Host) -> Unit,
    add: (Host) -> Unit,
    delete: (Host) -> Unit,
    openTailnet: () -> Unit,
) {
    val hosts = store.hosts
    val matches = remember(hosts, search) { matchingHosts(hosts, search) }
    val sections = remember(hosts, matches) { hostSections(hosts, matches) }
    val colors = GroupedColors(MaterialTheme.colorScheme.surface.luminance() < 0.5f)

    GroupedPage(
        tr("hosts"),
        onBack = onBack,
        trailing = { GlassIconButton("plus", tr("newhost")) { add(Host()) } },
    ) {
        GlassSearchField(search, tr("hosts.search"), Modifier.padding(horizontal = 16.dp, vertical = 6.dp), onSearch)

        if (hosts.isEmpty()) {
            HostsEmptyState { add(Host()) }
        } else if (matches.isEmpty()) {
            Text(
                tr("hosts.nomatch.q", search),
                fontSize = 15.sp,
                color = colors.secondary,
                textAlign = TextAlign.Center,
                modifier = Modifier.fillMaxWidth().padding(horizontal = 24.dp, vertical = 28.dp),
            )
        } else {
            val grouped = sections.size > 1 || sections.firstOrNull()?.title != null
            for (section in sections) {
                Group(if (grouped) section.title ?: tr("hosts.other") else null) {
                    for (host in section.hosts) {
                        HostListRow(
                            host = host,
                            line = hostLine(store, host),
                            session = Sessions.existing(host.id),
                            // Disconnected, not closed: a session the list no
                            // longer holds is one home connects again.
                            onDisconnect = { Sessions.existing(host.id)?.disconnect() },
                            onPage = { page(host) },
                            onOpen = { onOpen(host) },
                            onDuplicate = { store.duplicate(host, tr("host.copysuffix")) },
                            onForgetKey = { forgetHostKey(store, host.id) },
                            onDelete = { delete(host) },
                        )
                    }
                }
            }
        }

        // The tailnet ThinkTerm joined by itself, and its machines.
        TailnetSection(store, openTailnet, open = page, add = add)

        if (probe != null) {
            Group(tr("dev")) {
                GroupRow(onClick = { onOpen(probe) }) {
                    HostRowContent(tr("thismac"), "probe sshd on ${probe.hostname}:${probe.port}", session = Sessions.existing(Host.PROBE_ID)) {
                        DesktopIcon("app-window", size = 20.dp, tint = colors.primary)
                    }
                }
            }
        }
    }
}

/// One host: its picture, with a dot while its session is up or on its
/// way, its name and its address; a tap opens its page, a long press the
/// menu.
@Composable
private fun HostListRow(
    host: Host,
    line: String,
    session: TerminalModel?,
    onDisconnect: () -> Unit,
    onPage: () -> Unit,
    onOpen: () -> Unit,
    onDuplicate: () -> Unit,
    onForgetKey: () -> Unit,
    onDelete: () -> Unit,
) {
    val colors = groupedColors()
    var menu by remember { mutableStateOf(false) }
    Box {
        GroupRow(onClick = onPage, onLongClick = { menu = true }) {
            HostRowContent(host.title, line, monospaced = true, session = session) {
                DesktopIcon(host.iconFile, size = 21.dp, tint = colors.primary)
            }
            RowChevron()
        }
        DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
            DropdownMenuItem(text = { Text(tr("connect")) }, onClick = { menu = false; onOpen() })
            if (session?.isConnected == true) {
                DropdownMenuItem(text = { Text(tr("disconnect")) }, onClick = { menu = false; onDisconnect() })
            }
            DropdownMenuItem(text = { Text(tr("duplicate")) }, onClick = { menu = false; onDuplicate() })
            if (host.knownHost != null) {
                DropdownMenuItem(text = { Text(tr("forgetkey")) }, onClick = { menu = false; onForgetKey() })
            }
            HorizontalDivider()
            DropdownMenuItem(
                text = { Text(tr("delete"), color = colors.destructive) },
                onClick = { menu = false; onDelete() },
            )
        }
    }
}

/// Where a host's session stands, for its dot in the list and its page.
internal enum class SessionState {
    /// No session, or one that never got anywhere.
    NONE,
    /// Dialling, attaching, or redialing by itself.
    CONNECTING,
    CONNECTED,
    /// Lost, and not redialing (auto-reconnect off): the way back is a tap.
    LOST;

    /// The dot a host's picture wears: up, or on its way.
    val dot: Color?
        get() = when (this) {
            CONNECTED -> Color(0xFF34C759)
            CONNECTING -> Color(0xFFFF9F0A)
            NONE, LOST -> null
        }

    companion object {
        fun of(model: TerminalModel): SessionState = when {
            // A redial in its first quiet moments shows as the session did.
            model.reconnecting && !model.redialQuiet -> if (model.autoReconnect) CONNECTING else LOST
            model.connection.startsWith("pane ") -> CONNECTED
            model.isConnected -> CONNECTING
            else -> NONE
        }
    }
}

/// The square a host or a machine leads with: its picture on a quiet fill,
/// and the session's dot at the corner, cut out of the row's background.
@Composable
internal fun HostTile(size: Dp = 40.dp, session: TerminalModel? = null, mark: @Composable () -> Unit) {
    val colors = groupedColors()
    Box(Modifier.size(size)) {
        Box(
            Modifier.fillMaxSize().clip(RoundedCornerShape(size * 0.28f)).background(colors.fill),
            contentAlignment = Alignment.Center,
        ) { mark() }
        val dot = session?.let { SessionState.of(it).dot }
        if (dot != null) {
            Box(
                Modifier
                    .align(Alignment.BottomEnd)
                    .offset(3.dp, 3.dp)
                    .size(12.dp)
                    .background(colors.card, CircleShape)
                    .padding(2.5.dp)
                    .background(dot, CircleShape),
            )
        }
    }
}

/// The shape of a row: a tile, a name, and an address under it.
@Composable
internal fun RowScope.HostRowContent(
    title: String,
    subtitle: String,
    monospaced: Boolean = false,
    session: TerminalModel? = null,
    mark: @Composable () -> Unit,
) {
    val colors = groupedColors()
    HostTile(session = session, mark = mark)
    Spacer(Modifier.width(2.dp))
    Column(Modifier.weight(1f).padding(vertical = 4.dp)) {
        Text(title, color = colors.primary, fontSize = 16.sp, fontWeight = FontWeight.SemiBold, maxLines = 1, overflow = TextOverflow.Ellipsis)
        Text(
            subtitle,
            color = colors.secondary,
            fontSize = 13.sp,
            fontFamily = if (monospaced) FontFamily.Monospace else null,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
        )
    }
}

@Composable
internal fun HostsEmptyState(add: () -> Unit) {
    val secondary = MaterialTheme.colorScheme.onSurfaceVariant
    Column(
        Modifier.fillMaxWidth().padding(horizontal = 28.dp, vertical = 40.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        DesktopIcon("server", size = 46.dp, tint = secondary)
        Spacer(Modifier.height(14.dp))
        Text(tr("hosts.empty"), fontSize = 18.sp, fontWeight = FontWeight.SemiBold, color = MaterialTheme.colorScheme.onSurface)
        Spacer(Modifier.height(6.dp))
        Text(tr("hosts.empty.body"), fontSize = 15.sp, color = secondary, textAlign = TextAlign.Center)
        Spacer(Modifier.height(18.dp))
        Row(
            Modifier.fillMaxWidth().height(50.dp).clip(CircleShape).background(LeadBlue).clickable(onClick = add),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.Center,
        ) {
            DesktopIcon("plus", size = 20.dp, tint = Color.White)
            Spacer(Modifier.width(8.dp))
            Text(tr("hosts.add"), color = Color.White, fontSize = 16.sp, fontWeight = FontWeight.SemiBold)
        }
    }
}

private data class HostSection(val title: String?, val hosts: List<Host>)

private fun matchingHosts(hosts: List<Host>, search: String): List<Host> {
    val query = search.trim().lowercase()
    if (query.isEmpty()) return hosts
    return hosts.filter { host ->
        listOf(host.name, host.hostname, host.user, host.group).any { it.lowercase().contains(query) }
    }
}

/// One section per group once any host has one, ungrouped hosts last; a
/// single title-less section when nobody uses groups.
private fun hostSections(all: List<Host>, matches: List<Host>): List<HostSection> {
    if (all.none { it.group.isNotEmpty() }) return listOf(HostSection(null, matches))
    val byGroup = matches.groupBy { it.group }
    val out = byGroup.keys.filter { it.isNotEmpty() }.sorted()
        .map { HostSection(it, byGroup[it].orEmpty()) }
        .toMutableList()
    byGroup[""]?.takeIf { it.isNotEmpty() }?.let { out.add(HostSection(null, it)) }
    return out
}
