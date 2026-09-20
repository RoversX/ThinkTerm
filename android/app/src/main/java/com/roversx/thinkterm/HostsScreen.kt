package com.roversx.thinkterm

import android.text.format.DateUtils
import androidx.activity.compose.BackHandler
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
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Add
import androidx.compose.material.icons.filled.Bolt
import androidx.compose.material.icons.filled.PowerSettingsNew
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.ContentCopy
import androidx.compose.material.icons.filled.Delete
import androidx.compose.material.icons.filled.Dns
import androidx.compose.material.icons.filled.Edit
import androidx.compose.material.icons.filled.GppBad
import androidx.compose.material.icons.filled.Hardware
import androidx.compose.material.icons.filled.Search
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.LargeTopAppBar
import androidx.compose.material3.ListItem
import androidx.compose.material3.ListItemDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp

/// The saved hosts, searched and grouped, plus — in a debuggable build —
/// the entry that reaches the probe sshd on the developer's Mac. The
/// editor takes the whole screen; system back closes it.
/// Follows ios/Sources/HostsView.swift.
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun HostsScreen(store: HostStore, showProbe: Boolean, onOpen: (Host) -> Unit) {
    var editing by remember { mutableStateOf<Host?>(null) }
    var search by remember { mutableStateOf("") }
    var pendingDelete by remember { mutableStateOf<Host?>(null) }

    val target = editing
    if (target != null) {
        BackHandler { editing = null }
        HostEditScreen(
            store = store,
            host = target,
            onSave = { host, secret, passphrase ->
                // A host with nothing stored needs this save to write the
                // secret; an empty field on an edit keeps the stored one.
                if (secret.isNotEmpty() || store.secret(host.id).isEmpty()) {
                    store.putSecret(host.id, secret, passphrase)
                } else if (passphrase != store.passphrase(host.id)) {
                    // Only the passphrase changed: kept with the key it unlocks.
                    store.putSecret(host.id, store.secret(host.id), passphrase)
                }
                store.upsert(host)
                editing = null
            },
            onCancel = { editing = null },
        )
        return
    }

    val hosts = store.hosts
    val matches = remember(hosts, search) { matchingHosts(hosts, search) }
    val sections = remember(hosts, matches) { hostSections(hosts, matches) }

    Scaffold(
        containerColor = MaterialTheme.colorScheme.background,
        topBar = {
            LargeTopAppBar(
                title = { Text(tr("hosts")) },
                actions = {
                    IconButton(onClick = { editing = Host() }) {
                        Icon(Icons.Filled.Add, contentDescription = tr("hosts.add"))
                    }
                },
            )
        },
    ) { inset ->
        LazyColumn(
            Modifier.fillMaxSize().padding(inset),
            contentPadding = PaddingValues(bottom = 28.dp),
        ) {
            item { HostSearchField(search) { search = it } }

            if (hosts.isEmpty()) {
                item { HostsEmptyState { editing = Host() } }
            } else if (matches.isEmpty()) {
                item {
                    Text(
                        tr("hosts.nomatch.q", search),
                        style = MaterialTheme.typography.bodyMedium,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                        textAlign = TextAlign.Center,
                        modifier = Modifier.fillMaxWidth().padding(horizontal = 24.dp, vertical = 28.dp),
                    )
                }
            } else {
                val grouped = sections.size > 1 || sections.firstOrNull()?.title != null
                for (section in sections) {
                    if (grouped) {
                        item(key = "g:" + (section.title ?: "")) {
                            HostGroupHeader(section.title ?: tr("hosts.other"))
                        }
                    }
                    items(section.hosts, key = { it.id }) { host ->
                        HostListRow(
                            host = host,
                            connected = Sessions.isConnected(host.id),
                            onDisconnect = { Sessions.close(host.id) },
                            onOpen = { onOpen(host) },
                            onEdit = { editing = host },
                            onDuplicate = { store.duplicate(host, tr("host.copysuffix")) },
                            onForgetKey = { store.forgetHostKey(host.id) },
                            onDelete = { pendingDelete = host },
                        )
                    }
                }
            }

            if (showProbe) {
                item(key = "dev-header") { HostGroupHeader(tr("dev")) }
                item(key = Host.PROBE_ID) {
                    ListItem(
                        modifier = Modifier.clickable { onOpen(Host.probe) },
                        colors = ListItemDefaults.colors(containerColor = Color.Transparent),
                        leadingContent = { HostDisc(Icons.Filled.Hardware, Color(0xFF8E8E93)) },
                        headlineContent = {
                            Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                                if (Sessions.isConnected(Host.PROBE_ID)) {
                                    Box(Modifier.size(8.dp).background(Color(0xFF4CAF50), CircleShape))
                                }
                                Text(tr("thismac"), fontWeight = FontWeight.SemiBold)
                            }
                        },
                        supportingContent = {
                            Text("probe sshd on ${Host.probe.hostname}:${Host.probe.port}")
                        },
                    )
                }
            }
        }
    }

    pendingDelete?.let { host ->
        AlertDialog(
            onDismissRequest = { pendingDelete = null },
            title = { Text(tr("host.delete.title")) },
            text = { Text(tr("host.delete.body")) },
            confirmButton = {
                TextButton(onClick = { Sessions.close(host.id); store.remove(host); pendingDelete = null }) {
                    Text(tr("host.delete.confirm", host.display), color = MaterialTheme.colorScheme.error)
                }
            },
            dismissButton = {
                TextButton(onClick = { pendingDelete = null }) { Text(tr("cancel")) }
            },
        )
    }
}

@Composable
private fun HostSearchField(search: String, onChange: (String) -> Unit) {
    OutlinedTextField(
        value = search,
        onValueChange = onChange,
        placeholder = { Text(tr("hosts.search")) },
        singleLine = true,
        leadingIcon = { Icon(Icons.Filled.Search, contentDescription = null) },
        trailingIcon = {
            if (search.isNotEmpty()) {
                IconButton(onClick = { onChange("") }) {
                    Icon(Icons.Filled.Close, contentDescription = tr("cancel"))
                }
            }
        },
        modifier = Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 4.dp),
    )
}

@Composable
private fun HostGroupHeader(title: String) {
    Text(
        title,
        style = MaterialTheme.typography.labelLarge,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
        modifier = Modifier.padding(start = 16.dp, end = 16.dp, top = 18.dp, bottom = 4.dp),
    )
}

/// One host: the disc, the name, the address, and a line with when it was
/// last reached and how it authenticates. Long press opens the menu.
@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun HostListRow(
    host: Host,
    connected: Boolean,
    onDisconnect: () -> Unit,
    onOpen: () -> Unit,
    onEdit: () -> Unit,
    onDuplicate: () -> Unit,
    onForgetKey: () -> Unit,
    onDelete: () -> Unit,
) {
    var menu by remember { mutableStateOf(false) }
    Box {
        ListItem(
            modifier = Modifier.combinedClickable(onClick = onOpen, onLongClick = { menu = true }),
            colors = ListItemDefaults.colors(containerColor = Color.Transparent),
            leadingContent = { HostDisc(Icons.Filled.Dns, host.tint) },
            headlineContent = {
                Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                    if (connected) {
                        // Connected: the session is open behind the list.
                        Box(Modifier.size(8.dp).background(Color(0xFF4CAF50), CircleShape))
                    }
                    Text(host.title, maxLines = 1, overflow = TextOverflow.Ellipsis, fontWeight = FontWeight.SemiBold)
                }
            },
            supportingContent = {
                Column {
                    Text(host.address, maxLines = 1, overflow = TextOverflow.Ellipsis)
                    Row(
                        verticalAlignment = Alignment.CenterVertically,
                        horizontalArrangement = Arrangement.spacedBy(6.dp),
                        modifier = Modifier.padding(top = 3.dp),
                    ) {
                        Text(
                            host.lastConnectedText,
                            style = MaterialTheme.typography.labelSmall,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                        AuthBadge(host)
                    }
                }
            },
            trailingContent = {
                IconButton(onClick = onEdit) {
                    Icon(Icons.Filled.Edit, contentDescription = tr("edit"))
                }
            },
        )
        DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
            DropdownMenuItem(
                text = { Text(tr("connect")) },
                leadingIcon = { Icon(Icons.Filled.Bolt, contentDescription = null) },
                onClick = { menu = false; onOpen() },
            )
            if (connected) {
                DropdownMenuItem(
                    text = { Text(tr("disconnect")) },
                    leadingIcon = { Icon(Icons.Filled.PowerSettingsNew, contentDescription = null) },
                    onClick = { menu = false; onDisconnect() },
                )
            }
            DropdownMenuItem(
                text = { Text(tr("edit")) },
                leadingIcon = { Icon(Icons.Filled.Edit, contentDescription = null) },
                onClick = { menu = false; onEdit() },
            )
            DropdownMenuItem(
                text = { Text(tr("duplicate")) },
                leadingIcon = { Icon(Icons.Filled.ContentCopy, contentDescription = null) },
                onClick = { menu = false; onDuplicate() },
            )
            if (host.knownHost != null) {
                DropdownMenuItem(
                    text = { Text(tr("forgetkey")) },
                    leadingIcon = { Icon(Icons.Filled.GppBad, contentDescription = null) },
                    onClick = { menu = false; onForgetKey() },
                )
            }
            HorizontalDivider()
            DropdownMenuItem(
                text = { Text(tr("delete"), color = MaterialTheme.colorScheme.error) },
                leadingIcon = {
                    Icon(Icons.Filled.Delete, contentDescription = null, tint = MaterialTheme.colorScheme.error)
                },
                onClick = { menu = false; onDelete() },
            )
        }
    }
}

@Composable
private fun AuthBadge(host: Host) {
    Text(
        if (host.auth == "key") tr("badge.key") else tr("badge.password"),
        style = MaterialTheme.typography.labelSmall,
        fontWeight = FontWeight.SemiBold,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
        modifier = Modifier
            .background(MaterialTheme.colorScheme.onSurface.copy(alpha = 0.16f), CircleShape)
            .padding(horizontal = 7.dp, vertical = 2.dp),
    )
}

/// The tinted disc a row leads with; the gradient is the iOS row's.
@Composable
private fun HostDisc(icon: ImageVector, tint: Color) {
    Box(
        Modifier
            .size(40.dp)
            .background(Brush.verticalGradient(listOf(tint, tint.copy(alpha = 0.72f))), CircleShape),
        contentAlignment = Alignment.Center,
    ) {
        Icon(icon, contentDescription = null, tint = Color.White, modifier = Modifier.size(19.dp))
    }
}

@Composable
private fun HostsEmptyState(add: () -> Unit) {
    Column(
        Modifier.fillMaxWidth().padding(horizontal = 28.dp, vertical = 40.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        Icon(
            Icons.Filled.Dns,
            contentDescription = null,
            tint = MaterialTheme.colorScheme.onSurfaceVariant,
            modifier = Modifier.size(46.dp),
        )
        Spacer(Modifier.height(14.dp))
        Text(tr("hosts.empty"), style = MaterialTheme.typography.titleMedium)
        Spacer(Modifier.height(6.dp))
        Text(
            tr("hosts.empty.body"),
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            textAlign = TextAlign.Center,
        )
        Spacer(Modifier.height(18.dp))
        Button(onClick = add, modifier = Modifier.fillMaxWidth()) {
            Icon(Icons.Filled.Add, contentDescription = null)
            Spacer(Modifier.width(8.dp))
            Text(tr("hosts.add"))
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

private val HOST_PALETTE = listOf(
    Color(0xFF0A84FF), // blue
    Color(0xFF5E5CE6), // indigo
    Color(0xFFBF5AF2), // purple
    Color(0xFFFF375F), // pink
    Color(0xFFFF9F0A), // orange
    Color(0xFF40CBE0), // teal
    Color(0xFF30D158), // green
    Color(0xFF64D2FF), // cyan
)

/// A stable colour per host: the djb2 hash the iOS app uses, so a host
/// wears the same colour on both phones.
private val Host.tint: Color
    get() {
        var hash = 5381UL
        for (byte in (name + hostname + user).toByteArray()) {
            hash = hash * 33UL + byte.toUByte().toULong()
        }
        return HOST_PALETTE[(hash % HOST_PALETTE.size.toULong()).toInt()]
    }

/// "13m ago", or the never-connected line. The phone's locale formats it;
/// iOS follows the app's language, which Android has no formatter for.
private val Host.lastConnectedText: String
    get() {
        val last = lastConnected ?: return tr("hosts.never")
        return DateUtils.getRelativeTimeSpanString(
            last,
            System.currentTimeMillis(),
            DateUtils.MINUTE_IN_MILLIS,
        ).toString()
    }
